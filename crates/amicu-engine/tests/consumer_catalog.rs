//! Consumer/on-prem hardware catalog entries resolve through the existing
//! `Catalog`/`MapCatalog` hardware-profile lookup path (the same one
//! `amicu-llamacpp`'s adapter uses), and pass the existing weight-fit check

use amicu_core::resource::Quantization;
use amicu_llamacpp::{Catalog, MapCatalog};
use amicu_providers::consumer;
use amicu_registry::{FitConfig, check_fit};

/// A consumer catalog entry, registered in a [`MapCatalog`], resolves back
/// out through [`Catalog::resolve_hardware`] - the same lookup path
/// `amicu-llamacpp`'s adapter drives a deployment's `hardware_profile`
/// reference through.
#[test]
fn consumer_entry_resolves_through_map_catalog() {
    let hw = consumer::rtx_4080();
    let catalog = MapCatalog::new().with_hardware(hw.clone());

    let resolved = catalog
        .resolve_hardware(&hw.id)
        .expect("registered hardware profile should resolve");

    assert_eq!(resolved, hw);
}

/// A 7B model at Q4_K_M fits comfortably on a 16 GB consumer GPU.
#[test]
fn seven_b_q4km_fits_on_sixteen_gb_consumer_gpu() {
    let hw = consumer::rtx_4080();
    let report = check_fit(
        7_000_000_000,
        Quantization::Q4KM,
        &hw,
        &FitConfig::default(),
    );
    assert!(
        report.fits,
        "7B Q4_K_M should fit a 16 GB card, got: {report:?}"
    );
}

/// A 70B model at Q4_K_M does NOT fit on a 24 GB consumer GPU.
#[test]
fn seventy_b_q4km_does_not_fit_on_twenty_four_gb_consumer_gpu() {
    let hw = consumer::rtx_4090();
    let report = check_fit(
        70_000_000_000,
        Quantization::Q4KM,
        &hw,
        &FitConfig::default(),
    );
    assert!(
        !report.fits,
        "70B Q4_K_M should not fit a 24 GB card, got: {report:?}"
    );
}

/// The same 70B Q4_K_M model fits on an M3 Ultra 256 GB unified-memory
/// profile.
#[test]
fn seventy_b_q4km_fits_on_m3_ultra_256gb() {
    let hw = consumer::apple_silicon_chip("m3_ultra")
        .and_then(|c| c.profile(256))
        .expect("m3_ultra has a 256 GiB configuration");
    let report = check_fit(
        70_000_000_000,
        Quantization::Q4KM,
        &hw,
        &FitConfig::default(),
    );
    assert!(
        report.fits,
        "70B Q4_K_M should fit an M3 Ultra 256 GB profile, got: {report:?}"
    );
}

/// Every catalog entry resolves through `MapCatalog` and (trivially) matches
/// itself - a broad smoke test that the whole catalog is usable as hardware
/// profiles, not just the four entries exercised individually above.
#[test]
fn every_catalog_entry_resolves() {
    let entries = consumer::catalog();
    let mut catalog = MapCatalog::new();
    for hw in &entries {
        catalog = catalog.with_hardware(hw.clone());
    }
    for hw in &entries {
        assert_eq!(catalog.resolve_hardware(&hw.id).as_ref(), Some(hw));
    }
}
