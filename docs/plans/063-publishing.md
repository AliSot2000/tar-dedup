# 063 — Publishing: crates.io, cargo-dist, distro ports, site

Status: draft for approval. Renumbered from 062 when the crate-rename plan (062)
took the lower number. Crate references assume the rename of `062` is applied:
lib = `tar-dedup-lib`, CLI = `tar-dedup`.

## Confirmed distribution strategy (recap, settled 2026-10)

- **crates.io** is the primary artifact source. Publish order: `sparse-cp` →
  `tar-dedup-lib` → `tar-dedup` (the CLI depends on both).
- **cargo-dist** (GitHub Releases) produces per-platform **tarballs** for
  manual installation plus the universal **install script** and a **Homebrew
  formula** (with Linux builds, fed into the tap).
- **Linux ports** are bot-maintained on top of the crates.io releases — no
  self-hosted apt/dnf repositories: Debian via debcargo, Fedora via
  rust2rpm (+Copr/pagure), Arch AUR via the rust bot. Same for every other
  distro that tracks crates.io.
- **Windows is blocked** for now: `nix`, `posix-acl`, `xattrs`, `selinux` are
  unconditional unix-only dependencies.
- **Flatpak / winget / choco**: deferred (Flatpak = optional later milestone;
  winget/choco = one-time bot submissions once stable releases exist).

## What is currently missing (the gaps)

1. **License metadata**: `[workspace.package]` has only `version`/`edition`/
   `rust-version`. No `license`, `description` (`sparse-cp` has a description;
   `tar-dedup`/`tar-dedup-cli` do not), `repository`, `homepage`, `authors`.
   `LICENSE.txt` (MIT, © 2026 Alexander Sotoudeh) exists but is not wired into
   any manifest.
2. **AI.md**: there is `AGENTS.md` (agent guide) but no `AI.md` (the
   agent/scraper-facing one-pager that common tools look up at the site root).
3. **robots.txt**: missing, and there is no publish location / homepage for it
   to live at.
4. **Publish location**: no public hosting decision recorded — the `repository`
   and `homepage` fields are empty; crates.io requires `license` +
   `description` and strongly prefers `repository`.

## 1. Manifest metadata (`Cargo.toml`)

Add to `[workspace.package]` (inherited by all three crates via
`license.workspace = true` etc.):

- `license = "MIT"` (single-expression; `LICENSE.txt` stays as the full text).
- `description` per crate:
  - `tar-dedup-lib` — deduplicating `tar` wrapper library for efficient cold
    storage.
  - `tar-dedup` — command-line interface for `tar-dedup-lib` (resumable,
    metadata-preserving archive/extract).
  - `sparse-cp` — already present.
- `repository = "https://github.com/<owner>/tar-dedup"` (confirm owner).
- `homepage` = the GitHub Pages site (see §6) once it exists, else the repo URL.
- `authors = ["Alexander Sotoudeh"]`.
- `readme`: crates.io renders a README per package. The repo README lives at
  the workspace root; decide between (a) a small per-crate `Readme.md`
  (recommended; crates.io requires it inside the package) or (b) trying
  `readme = "../Readme.md"` and validating with `cargo package`.

Validation: `cargo publish --dry-run` per crate, plus a manual look at
`cargo package --list` for the published file set (LICENSE must be included —
Cargo includes it automatically when `license`/`license-file` is set and it
ships with the crate).

## 2. Publish order and ownership

- `sparse-cp` first (no deps on the other two), then `tar-dedup-lib`, then
  `tar-dedup` (CLI).
- **Name availability**: `sparse-cp`, `tar-dedup-lib`, `tar-dedup` on
  crates.io were checked at plan time and are unclaimed; re-check before the
  first dry-run. If `tar-dedup` is claimed by then, decide a fallback (e.g.
  `tar-dedup-tool`) before publishing.
- Publish with `cargo publish -p <crate>` sequentially; the CLI must not be
  yanked if a lib release is corrected (semver discipline: 0.x = breaking
  tolerated, but prefer not editing published versions).

## 3. cargo-dist scaffold

- `cargo dist init` in the workspace → generates `.github/workflows/release.yml`
  + `dist-workspace.toml` when merged; builds on tag `v*`.
- Artifacts: `install.sh`, per-platform tarballs (x86_64/aarch64 for Linux and
  macOS), and the Homebrew formula.
- Confirm macOS build actually succeeds before flying the formula (the only
  unix-y dependency set that builds on macOS: `posix-acl`/`selinux` must be
  feature-gated or build-optional on macOS — **open question**, see §8).
- `dist-workspace.toml`: pin the published crates as the source-of-truth rather
  than git tarballs where possible, so the GitHub release matches crates.io.

## 4. Homebrew

- cargo-dist emits the formula; merge requests into a tap
  (`homebrew-tar-dedup` or the core tap if maintainers accept it).
- Formula must cover both Linux and macOS bottles (Linux distro users + macOS
  users get the same signal; this does NOT replace the distro-native ports).

## 5. Linux distro ports (bot-maintained)

- Document, in `Readme.md` (Install section), the crates.io-first ingestion:
  - Debian/Ubuntu: `debcargo` watch entry.
  - Fedora/RHEL: `rust2rpm` + package review (Rust package review process),
    later Copr for pre-release.
  - Arch: AUR `tar-dedup` (rust bot from crates.io release).
  - OpenSUSE: `cargo2rpm`; Gentoo: `cargo-ebuild` — same crates.io source.
- Constraint to record: edition **2024** + `rust-version = 1.95` (and `nix`
  `0.29`) set the minimum toolchain for every distro; Debian/Fedora ship
  current Rust so this is fine, but note it in the packaging notes.
- Own the maintenance burden explicitly: we submit nothing by hand after the
  initial seeds; the distros' bots consume crates.io tags. A "known package
  sources" table goes into Readme.

## 6. Publish location / website (robots.txt)

- **GitHub public repo** is the canonical "publish location"
  (`repository` field). 
- `homepage` = a **GitHub Pages** site served from the default-branch root of
  the repo (Readme.md renders at the site root).
- Add `robots.txt` at the repo root (site root) with an allow-all policy and a
  `Sitemap:` comment reserved for later:
  ```
  User-agent: *
  Allow: /
  ```
- Confirm GitHub Pages serving-from-root works before promising the URL in
  `homepage` (Pages now supports serving the root of a branch).

## 7. AI.md

- New `AI.md` at the repo root: a short (≤ ~60 line) agent/scraper one-pager —
  what the project is, the two pipelines, where the documentation lives
  (`Readme.md`, `AGENTS.md`, `docs/plans/`), build/test/dev commands,
  contribution pointers. Cross-links to `AGENTS.md` for the full agent guide
  instead of duplicating it.
- Kept in sync with `AGENTS.md`/`Readme.md` by convention (a short checklist
  note, not a generated file).

## 8. Open questions (to settle)

1. GitHub owner/org for the public repo and Pages URL (`alisot2000`?).
2. `tar-dedup` name availability on crates.io (blocking if taken).
3. Per-crate `Readme.md` vs `readme = "../Readme.md"` (validation task).
4. macOS build viability: the unconditional `selinux`/`posix-acl`/`xattrs`
   deps may break `cargo dist` macOS artifacts — gate on macOS or accept
   Linux-only bottles for the first release?
5. `homepage`: Pages site vs plain repo URL (affects where `robots.txt` sits).

## Implementation order

1. Cargo metadata (#1) + `cargo publish --dry-run` + name checks (#2).
2. Publish the three crates.
3. `cargo dist init`, macOS check, GitHub Actions release workflow, tag v0.1.0.
4. GitHub repo to public + Pages + `robots.txt` (#6).
5. `AI.md` (#7).
6. Readme Install section with the distro-port table (#5).
7. Settle Flatpak/windows-tracker follow-up issues separately.