# Ratty Bitmap Draft PR History Rewrite Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace Ratty's 24-commit bitmap development history with six review-oriented commits that produce the same approved feature patch, verify the result, and push it without opening a pull request.

**Architecture:** Preserve `44a030a` behind a backup branch, reconstruct the final feature by responsibility on top of current `upstream/main` (`04180e9`), and prove that every feature-owned path matches the approved source tip. Rendering and terminal-compatibility changes that share `src/inline.rs` and `src/systems.rs` are separated by reversing only the compatibility range for the rendering commit and restoring the approved final files for the compatibility commit.

**Tech Stack:** Git, Rust 2024, Cargo, Cargo nextest, GitHub fork remotes.

## Global Constraints

- Work only in `/Users/atasesli/Desktop/VsCode/ratty` for the rewrite.
- Do not modify Ratty source code; reconstruct existing approved content only.
- Approved source tip: `44a030a`.
- Original feature base: `697d3e0`.
- Current upstream base: `04180e9`.
- Pre-compatibility feature tip: `5c15aa9`.
- Formatted compatibility tip: `d65a8ad`.
- Create local backup branch `backup/bitmap-surface-pre-review-20260716` before rewriting.
- Build on temporary branch `rewrite/bitmap-surface-review` before moving `feature/bitmap-surface`.
- Never use plain `--force`; push only with an exact `--force-with-lease` expectation.
- Stop before creating a draft pull request; the user will open it manually.
- Keep the backup branch after pushing.
- Use `cargo nextest run`, not `cargo test`.

---

## File Structure

- `protocols/bitmap.md`: complete `ratty;i` wire protocol and safety limits.
- `protocols/graphics.md`: distinction between bitmap surfaces and RGP.
- `src/bitmap.rs`: protocol parser, bounded transfers, bitmap state, placement lifecycle, and tests.
- `src/bitmap_material.rs`: Bevy material and placement layout calculations.
- `src/shaders/bitmap_surface.wgsl`: bitmap sampling shader.
- `src/inline.rs`: APC ingestion, render caches, Kitty compatibility replies, and tests.
- `src/plugin.rs`: shader/material/system registration.
- `src/systems.rs`: GPU upload, placement synchronization, cleanup, delta sync, and callback refresh.
- `src/kitty.rs`: Kitty query parsing and validation.
- `src/runtime.rs`: Ratty child identity and `CSI 16t` reply handling.
- `src/terminal.rs`: terminal-cell pixel dimensions.
- `src/lib.rs`: bitmap module exports.
- `examples/bitmap_pan_zoom.rs`: static placement, pan, zoom, crop, and fit demonstration.
- `examples/bitmap_frames.rs`: live RGBA frame demonstration.
- `README.md`: user-facing bitmap feature and example documentation.

---

### Task 1: Establish immutable safety refs and the current base

**Files:**
- Modify: Git refs only

**Interfaces:**
- Consumes: approved source tip `44a030a`, current remote refs
- Produces: local backup branch and clean temporary branch based on `04180e9`

- [ ] **Step 1: Confirm the source and worktree are unchanged**

Run:

```bash
git status --short
git rev-parse HEAD
git rev-parse upstream/main
```

Expected: empty status, `HEAD` is `44a030a...`, and `upstream/main` is `04180e9...`.

- [ ] **Step 2: Confirm upstream did not modify feature-owned paths**

Run:

```bash
git diff --name-only 697d3e0..04180e9
```

Expected: only `config/ratty.toml`, `src/config.rs`, `src/keyboard.rs`, `src/mouse.rs`, `website/index.html`, `widget/Cargo.lock`, and `widget/Cargo.toml`; none overlap the bitmap feature paths.

- [ ] **Step 3: Create the immutable local backup branch**

Run:

```bash
git branch backup/bitmap-surface-pre-review-20260716 44a030a
```

Expected: the backup branch points exactly to `44a030a`.

- [ ] **Step 4: Create the temporary review branch**

Run:

```bash
git switch -c rewrite/bitmap-surface-review 04180e9
```

Expected: clean branch at current upstream `main`.

---

### Task 2: Reconstruct protocol and core state commits

**Files:**
- Modify: `protocols/bitmap.md`
- Modify: `protocols/graphics.md`
- Create: `src/bitmap.rs`
- Modify: `src/lib.rs`

**Interfaces:**
- Consumes: final content from `backup/bitmap-surface-pre-review-20260716`
- Produces: documented protocol plus bounded parser/state module exported as `bitmap`

- [ ] **Step 1: Restore and commit the final protocol documentation**

Run:

```bash
git restore --source=backup/bitmap-surface-pre-review-20260716 --worktree -- protocols/bitmap.md protocols/graphics.md
git add protocols/bitmap.md protocols/graphics.md
git diff --cached --check
git commit -m "docs(bitmap): specify the bitmap surface protocol"
```

Expected: one documentation-only commit with the complete final protocol text.

- [ ] **Step 2: Restore the final bitmap parser and state module**

Run:

```bash
git restore --source=backup/bitmap-surface-pre-review-20260716 --worktree -- src/bitmap.rs
git add src/bitmap.rs
```

Then add only this export to `src/lib.rs` immediately before `pub mod cli;`:

```rust
pub mod bitmap;
```

- [ ] **Step 3: Verify and commit the core module**

Run:

```bash
git add src/lib.rs
git diff --cached --check
cargo check
git commit -m "feat(bitmap): parse and manage bitmap surfaces"
```

Expected: Cargo check succeeds and the commit contains only `src/bitmap.rs` plus the single `bitmap` module export.

---

### Task 3: Reconstruct the final renderer without terminal compatibility

**Files:**
- Create: `src/bitmap_material.rs`
- Create: `src/shaders/bitmap_surface.wgsl`
- Modify: `src/inline.rs`
- Modify: `src/plugin.rs`
- Modify: `src/systems.rs`
- Modify: `src/lib.rs`

**Interfaces:**
- Consumes: `BitmapSurfaceState` and final approved renderer files
- Produces: Bevy material, shader, GPU upload/lifecycle, placement rendering, and delta synchronization

- [ ] **Step 1: Restore the final renderer-owned files**

Run:

```bash
git restore --source=backup/bitmap-surface-pre-review-20260716 --worktree -- src/bitmap_material.rs src/shaders/bitmap_surface.wgsl src/inline.rs src/plugin.rs src/systems.rs src/lib.rs
```

Expected: the working tree contains the final approved renderer and compatibility content.

- [ ] **Step 2: Remove only the later compatibility range from shared renderer files**

Run:

```bash
git diff 5c15aa9 d65a8ad -- src/inline.rs src/systems.rs | git apply --reverse
```

Expected: Kitty query reply handling and parser callback cell-size refresh are absent, while bitmap APC parsing, render caches, lifecycle handling, and delta synchronization remain.

- [ ] **Step 3: Verify and commit the renderer**

Run:

```bash
git add src/bitmap_material.rs src/shaders/bitmap_surface.wgsl src/inline.rs src/plugin.rs src/systems.rs src/lib.rs
git diff --cached --check
cargo check
git commit -m "feat(bitmap): render and synchronize bitmap surfaces"
```

Expected: Cargo check succeeds and the commit contains no `src/kitty.rs`, `src/runtime.rs`, or `src/terminal.rs` changes.

---

### Task 4: Restore terminal-client compatibility as one commit

**Files:**
- Modify: `src/inline.rs`
- Modify: `src/kitty.rs`
- Modify: `src/runtime.rs`
- Modify: `src/systems.rs`
- Modify: `src/terminal.rs`

**Interfaces:**
- Consumes: renderer from Task 3 and exact final content from the backup tip
- Produces: Kitty query replies, Ratty terminal identity, `CSI 16t`, and current cell-size callback refresh

- [ ] **Step 1: Restore the exact final compatibility files**

Run:

```bash
git restore --source=backup/bitmap-surface-pre-review-20260716 --worktree -- src/inline.rs src/kitty.rs src/runtime.rs src/systems.rs src/terminal.rs
git add src/inline.rs src/kitty.rs src/runtime.rs src/systems.rs src/terminal.rs
```

Expected: the staged diff contains only the compatibility range removed in Task 3 plus the three compatibility-focused files.

- [ ] **Step 2: Verify and commit compatibility**

Run:

```bash
git diff --cached --check
cargo check
git commit -m "fix(terminal): advertise bitmap-compatible client capabilities"
```

Expected: Cargo check succeeds and Ratty answers the Kitty and cell-size probes used by terminal image clients.

---

### Task 5: Add final examples and README documentation

**Files:**
- Create: `examples/bitmap_pan_zoom.rs`
- Create: `examples/bitmap_frames.rs`
- Modify: `README.md`

**Interfaces:**
- Consumes: complete protocol, renderer, and compatibility implementation
- Produces: runnable demonstrations and user-facing discovery documentation

- [ ] **Step 1: Restore and commit both final examples**

Run:

```bash
git restore --source=backup/bitmap-surface-pre-review-20260716 --worktree -- examples/bitmap_pan_zoom.rs examples/bitmap_frames.rs
git add examples/bitmap_pan_zoom.rs examples/bitmap_frames.rs
git diff --cached --check
cargo check --examples
git commit -m "examples(bitmap): demonstrate placement and live frames"
```

Expected: both examples compile and the commit contains no production source files.

- [ ] **Step 2: Restore and commit final README usage documentation**

Run:

```bash
git restore --source=backup/bitmap-surface-pre-review-20260716 --worktree -- README.md
git add README.md
git diff --cached --check
git commit -m "docs(bitmap): document bitmap surface usage"
```

Expected: a README-only sixth commit.

---

### Task 6: Prove tree identity and run full verification

**Files:**
- Modify: none

**Interfaces:**
- Consumes: reconstructed six-commit branch
- Produces: evidence that feature-owned files equal `44a030a` and all Ratty checks pass

- [ ] **Step 1: Confirm exact content identity for every feature-owned path**

Run:

```bash
git diff --exit-code 44a030a HEAD -- README.md examples/bitmap_frames.rs examples/bitmap_pan_zoom.rs protocols/bitmap.md protocols/graphics.md src/bitmap.rs src/bitmap_material.rs src/inline.rs src/kitty.rs src/lib.rs src/plugin.rs src/runtime.rs src/shaders/bitmap_surface.wgsl src/systems.rs src/terminal.rs
```

Expected: no output and exit code 0.

- [ ] **Step 2: Confirm the review history has exactly six commits**

Run:

```bash
git log --oneline --reverse 04180e9..HEAD
git diff --shortstat 04180e9...HEAD
git status --short
```

Expected: exactly the six approved commit subjects, `15 files changed, 5917 insertions(+), 12 deletions(-)`, and a clean status.

- [ ] **Step 3: Run formatting and strict lint verification**

Run:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
```

Expected: both commands exit successfully.

- [ ] **Step 4: Run the complete test suite and build**

Run:

```bash
cargo nextest run
cargo build
```

Expected: all tests pass and the development build succeeds.

- [ ] **Step 5: Run final diff hygiene checks**

Run:

```bash
git diff --check 04180e9...HEAD
git status --short
```

Expected: no whitespace errors and a clean worktree.

---

### Task 7: Replace and push the review branch safely

**Files:**
- Modify: Git refs only

**Interfaces:**
- Consumes: verified temporary review branch and origin's current feature ref
- Produces: rewritten `feature/bitmap-surface` on the fork, ready for manual draft PR creation

- [ ] **Step 1: Refresh and record the exact remote lease**

Run:

```bash
git fetch origin feature/bitmap-surface
git rev-parse origin/feature/bitmap-surface
```

Expected: record the exact returned SHA; at plan creation it was `5c15aa9`.

- [ ] **Step 2: Move the local feature branch to the verified review tip**

Run:

```bash
git branch -f feature/bitmap-surface rewrite/bitmap-surface-review
git switch feature/bitmap-surface
```

Expected: `feature/bitmap-surface` contains exactly six commits above `04180e9`; the backup still points to `44a030a`.

- [ ] **Step 3: Push using the exact recorded lease**

Run with the SHA returned by Step 1 substituted exactly:

```bash
git push --force-with-lease=refs/heads/feature/bitmap-surface:5c15aa9 origin feature/bitmap-surface
```

Expected: origin accepts the rewritten six-commit branch. If the remote SHA differs from `5c15aa9`, replace only the lease SHA with the value freshly returned by Step 1. If the push reports a stale lease, stop and inspect instead of retrying with plain force.

- [ ] **Step 4: Verify remote and local tips and stop before PR creation**

Run:

```bash
git fetch origin feature/bitmap-surface
git rev-parse HEAD
git rev-parse origin/feature/bitmap-surface
git log --oneline --reverse 04180e9..HEAD
git branch --list backup/bitmap-surface-pre-review-20260716 rewrite/bitmap-surface-review feature/bitmap-surface
```

Expected: local and remote feature tips match; the backup and temporary review branches remain; no pull request has been created.
