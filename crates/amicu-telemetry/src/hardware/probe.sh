#!/bin/sh
# amicu hardware probe (issue andypalmi/amicu-planning#179).
#
# One fixed, read-only script. It is embedded in the binary (`include_str!`)
# and run verbatim, locally or inside a Modal container through
# `ContainerExec`; nothing is ever interpolated into it. It prints each data
# source's raw output under a `@@amicu:<name>` header line and the Rust side
# (`amicu_telemetry::hardware`) parses it. A tool that is missing, fails or
# prints nothing simply produces no section. No vendor is privileged: every
# source below is tried independently.
#
# Output is bounded: each tool's output is cut at 64 KiB and every tool runs
# under `timeout 5` when the container has `timeout`.

LC_ALL=C
export LC_ALL
PATH="$PATH:/usr/local/nvidia/bin:/opt/rocm/bin:/usr/local/bin:/usr/bin:/bin"
export PATH

CAP=65536

have() { command -v "$1" >/dev/null 2>&1; }

tmo() {
    if have timeout; then
        timeout 5 "$@"
    else
        "$@"
    fi
}

# grab CMD ARGS...: run, keep stdout in $out (empty on failure), capped.
grab() {
    out=$(tmo "$@" 2>/dev/null) || out=
    out=$(printf '%s' "$out" | head -c "$CAP")
}

# emit NAME TEXT: print a section, unless TEXT is empty.
emit() {
    [ -n "$2" ] || return 0
    printf '@@amicu:%s\n%s\n' "$1" "$2"
}

# emit_file NAME PATH: print a small readable file as a section.
emit_file() {
    [ -r "$2" ] || return 0
    body=$(head -c 4096 "$2" 2>/dev/null) || return 0
    emit "$1" "$body"
}

printf '@@amicu:probe-v1\n'

# --- NVIDIA -----------------------------------------------------------
if have nvidia-smi; then
    grab nvidia-smi \
        --query-gpu=index,name,utilization.gpu,memory.used,memory.total,temperature.gpu,power.draw \
        --format=csv,noheader,nounits
    emit nvidia-smi "$out"
fi

# --- AMD: amd-smi, else rocm-smi, else amdgpu sysfs --------------------
amd=
if have amd-smi; then
    grab amd-smi metric --json
    amd=$out
    emit amd-smi "$out"
fi
if [ -z "$amd" ] && have rocm-smi; then
    grab rocm-smi --showuse --showmeminfo vram --showtemp --showpower --json
    emit rocm-smi "$out"
fi
for d in /sys/class/drm/card[0-9]*; do
    n=${d##*/}
    case "$n" in *-*) continue ;; esac
    dev="$d/device"
    [ -r "$dev/vendor" ] || continue
    vendor=$(head -c 16 "$dev/vendor" 2>/dev/null)
    [ "$vendor" = "0x1002" ] || continue
    body="vendor=$vendor"
    for f in gpu_busy_percent mem_info_vram_used mem_info_vram_total; do
        if [ -r "$dev/$f" ]; then
            body="$body
$f=$(head -c 32 "$dev/$f" 2>/dev/null)"
        fi
    done
    for h in "$dev"/hwmon/hwmon[0-9]*; do
        for f in temp1_input power1_average power1_input; do
            if [ -r "$h/$f" ]; then
                body="$body
$f=$(head -c 32 "$h/$f" 2>/dev/null)"
            fi
        done
        break
    done
    emit "drm-$n" "$body"
done

# --- Intel: xpu-smi ---------------------------------------------------
if have xpu-smi; then
    grab xpu-smi discovery -j
    emit xpu-smi-discovery "$out"
    ids=$(printf '%s' "$out" | grep -o '"device_id"[^0-9]*[0-9]*' | grep -o '[0-9]*$' | head -n 16)
    grab xpu-smi stats -j
    emit xpu-smi-stats "$out"
    for id in $ids; do
        grab xpu-smi discovery -d "$id" -j
        emit "xpu-smi-device-$id" "$out"
    done
fi

# --- Apple (local macOS host only; no sudo) ----------------------------
if [ "$(uname -s 2>/dev/null)" = "Darwin" ] && have ioreg; then
    out=$(ioreg -r -d 1 -w 0 -c IOAccelerator 2>/dev/null |
        grep -E '^\+-o |"(model|PerformanceStatistics)" =' | head -c "$CAP") || out=
    emit ioreg "$out"
fi

# --- Container / host memory and CPU ----------------------------------
emit_file cgroup2-memory-current /sys/fs/cgroup/memory.current
emit_file cgroup2-memory-max /sys/fs/cgroup/memory.max
emit_file cgroup2-cpu-stat /sys/fs/cgroup/cpu.stat
emit_file cgroup1-memory-usage /sys/fs/cgroup/memory/memory.usage_in_bytes
emit_file cgroup1-memory-limit /sys/fs/cgroup/memory/memory.limit_in_bytes
if [ -r /sys/fs/cgroup/cpuacct/cpuacct.usage ]; then
    emit_file cgroup1-cpuacct-usage /sys/fs/cgroup/cpuacct/cpuacct.usage
else
    emit_file cgroup1-cpuacct-usage /sys/fs/cgroup/cpu,cpuacct/cpuacct.usage
fi
if [ -r /proc/meminfo ]; then
    emit meminfo "$(grep -E '^(MemTotal|MemAvailable):' /proc/meminfo 2>/dev/null | head -n 4)"
fi

exit 0
