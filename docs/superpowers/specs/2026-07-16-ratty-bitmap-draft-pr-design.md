# Ratty Bitmap Draft PR Design

**Date:** 2026-07-16

## Goal

Turn the completed Ratty bitmap-surface branch into a maintainer-friendly history without changing its final implementation, then push the rewritten branch for the user to open as a draft pull request manually.

The pull request will present one cohesive feature: a documented bitmap-surface protocol, bounded parser and state machine, GPU rendering path, terminal-client compatibility, examples, and usage documentation.

## Current State

- Repository: `/Users/atasesli/Desktop/VsCode/ratty`
- Branch: `feature/bitmap-surface`
- Approved source tip: `44a030a`
- Upstream target: `upstream/main`
- Current change size: 15 files, 5,917 insertions, 12 deletions
- Worktree is clean.
- The maintainer has already encouraged a first version of bitmap support in Ratty.

## Scope

The rewrite changes commit history only. It must not change the final source tree or protocol behavior.

Included:

- Bitmap protocol specification and namespace documentation
- Bounded command parsing and bitmap/placement state management
- APC ingestion and terminal dirty/scroll tracking
- Bevy material, shader, GPU asset lifecycle, and delta synchronization
- Kitty capability-query compatibility, Ratty child identity, and `CSI 16t`
- Static placement and live-frame examples
- README usage documentation

Excluded:

- Additional optimization or refactoring
- RChat changes
- New Ratty behavior beyond the approved branch tip
- Opening the GitHub pull request; the user will do that manually

## Chosen PR Shape

Use one draft pull request with six review-oriented commits:

1. `docs(bitmap): specify the bitmap surface protocol`
   - `protocols/bitmap.md`
   - Bitmap/RGP namespace distinction in `protocols/graphics.md`

2. `feat(bitmap): parse and manage bitmap surfaces`
   - Bitmap command grammar, limits, decoding, state, lifecycle, and tests in `src/bitmap.rs`
   - Bitmap module export in `src/lib.rs`

3. `feat(bitmap): render and synchronize bitmap surfaces`
   - `src/bitmap_material.rs`
   - `src/shaders/bitmap_surface.wgsl`
   - Bitmap APC ingestion and render caches in `src/inline.rs`
   - Bevy plugin registration in `src/plugin.rs`
   - Upload, placement, cleanup, lifecycle, and delta synchronization in `src/systems.rs`
   - Bitmap material module export in `src/lib.rs`

4. `fix(terminal): advertise bitmap-compatible client capabilities`
   - Kitty query parsing and replies in `src/kitty.rs` and `src/inline.rs`
   - Ratty child environment and `CSI 16t` handling in `src/runtime.rs`
   - Cell pixel dimensions in `src/terminal.rs`
   - Parser callback refresh in `src/systems.rs`

5. `examples(bitmap): demonstrate placement and live frames`
   - `examples/bitmap_pan_zoom.rs`
   - `examples/bitmap_frames.rs`

6. `docs(bitmap): document bitmap surface usage`
   - README feature and example documentation

All iterative fix, cleanup, lint, validation, and delta-sync commits are folded into the commit that owns the affected behavior. No standalone formatting or optimization commit remains.

## Safe Rewrite Procedure

1. Fetch `upstream` and confirm the intended base branch.
2. Record the original tip SHA and create a local backup branch at that exact commit.
3. Create a temporary review branch from the selected upstream base.
4. Reconstruct the six commits from the approved final patch, using exact path and hunk staging where files contain both rendering and compatibility changes.
5. Require each reconstructed commit to be coherent and free of unrelated paths.
6. Compare the reconstructed final tree with the approved source tip. Except for upstream-base changes deliberately incorporated during the rewrite, the feature patch must be identical.
7. Run the full Ratty verification matrix.
8. Replace the local `feature/bitmap-surface` branch only after tree identity and verification succeed.
9. Push with `--force-with-lease`, never plain `--force`.
10. Keep the local backup branch until the user has opened and inspected the draft pull request.

If upstream changed a touched file, stop and reconcile the conflict explicitly rather than overwriting upstream code wholesale.

## Verification

Run from the Ratty checkout:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo nextest run
cargo build
```

Also verify:

```bash
git diff --check upstream/main...feature/bitmap-surface
git status --short
```

The worktree must be clean, the final branch patch must match the approved implementation, and no RChat files may appear in the Ratty history.

## Draft PR Handoff

The assistant will stop after rewriting, verifying, and pushing `feature/bitmap-surface`. The user will open the draft pull request manually.

The eventual draft PR should:

- Target `orhun/ratty:main`
- Use a title such as `feat: add bitmap surface protocol and GPU renderer`
- Explain that this is a separate `ratty;i` bitmap namespace, not a replacement for RGP or Kitty graphics
- Give reviewers an ordered file/commit walkthrough
- Call out protocol bounds, transfer cleanup, asset lifetimes, and live-frame delta synchronization as deliberate correctness and safety work
- Include the full verification results
- State that RChat already exercises inline previews and Ratty-native bitmap surfaces against this implementation
