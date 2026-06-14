<!--
Thanks for contributing to rive-rust! Please fill out the sections below and complete the checklist.
See CONTRIBUTING.md and BUILD.md for setup and conventions.
-->

## Summary

<!-- What does this PR do, and why? Link any related issue (e.g. "Closes #123"). -->

## Changes

<!-- A short bullet list of the notable changes. -->

-

## Checklist

- [ ] **Clippy is clean on both tiers** —
      `cargo clippy -p bevy-rive --features floor --all-targets -- -D warnings` and
      `cargo clippy -p bevy-rive --no-default-features --features zero_copy --all-targets -- -D warnings`.
- [ ] **No changes under `vendor/`** — the rive-runtime submodule stays pristine (no patches, ever).
- [ ] **Feature wired across all four layers** (if this adds/changes a runtime-control feature):
      C++ shim → FFI decls → safe wrapper → Bevy component + system.
- [ ] **[`docs/feature-support.md`](../blob/master/docs/feature-support.md) updated** if the
      feature coverage changed (added/promoted/removed a capability).
- [ ] **DCO sign-off** — every commit is signed off (`git commit -s`), adding a
      `Signed-off-by: Name <email>` trailer that certifies the
      [Developer Certificate of Origin](https://developercertificate.org/).

## Notes for reviewers

<!-- Anything that needs extra attention: tier-specific behavior, platform/backend coverage,
     manual testing performed, etc. -->
