// Consolidated into `components/wizard/stepper.tsx` (shared by both the
// classic and guided wizard flows) - nothing imports this module anymore.
// This file could not be removed from this sandbox (`rm`/`git rm` are
// blocked by a local deny-destructive hook); it is left as a re-export so no
// duplicate implementation remains. A maintainer should `git rm` this file.
export { Stepper } from '@/components/wizard/stepper';
