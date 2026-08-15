# Repository setup checklist

Everything in the repository itself is configured in code — the build, the self-test, the
installer, the release, and the website all run from `.github/workflows/`. What is left are the
settings that live in GitHub's own UI and can only be changed by someone with admin rights on
the repository. Automation tokens deliberately cannot do these.

Work down the list once and it is done for good.

## 1. Rename the repository → `kite`

**Settings → General → Repository name** → `kite` → **Rename**.

This is what makes the site land on `https://blobbyofficial.github.io/kite/`. GitHub keeps
redirects from the old name, so existing clones and links keep working. Afterwards, update the
local remote:

```
git remote set-url origin https://github.com/blobbyofficial/kite.git
```

Nothing in the code hardcodes the repository name — the website substitutes it at deploy time —
so the rename needs no follow-up commit.

## 2. Turn on Pages

**Settings → Pages → Build and deployment → Source: GitHub Actions.**

Then run the **Website** workflow once (Actions → Website → Run workflow) and the site goes live.
Every later push that touches `site/` redeploys it automatically.

The workflow already asks GitHub to enable Pages itself, but that call needs admin rights the
Actions token does not have, so the first switch has to be flipped by hand.

## 3. Description, website and topics

**Settings → General**, or the ⚙ next to *About* on the repository home page.

- **Description**: `A video editor that stays fast on a modest laptop. Free, open source, Windows.`
- **Website**: `https://blobbyofficial.github.io/kite/`
- **Topics**: `video-editor`, `video-editing`, `nle`, `rust`, `ffmpeg`, `windows`, `egui`,
  `wgpu`, `performance`, `desktop-app`

Tick **Releases** and **Packages: off** under the About panel so the latest installer is the
first thing visitors see.

## 4. Features

**Settings → General → Features**

| Setting | Value | Why |
|---|---|---|
| Issues | **on** | The bug template in `.github/ISSUE_TEMPLATE` collects what a report needs |
| Discussions | on, optional | Useful once there are users asking questions rather than filing bugs |
| Wiki | **off** | The docs live in `docs/` and stay versioned with the code |
| Projects | off | Nothing to track yet |

## 5. Pull requests

**Settings → General → Pull Requests**

- Allow **squash merging** only — keeps history readable
- **Automatically delete head branches** — on

## 6. Security

**Settings → Advanced Security** — all free on a public repository:

- Dependabot alerts and security updates: **on**
- Secret scanning and push protection: **on**

## 7. Protect `main` (optional, recommended)

**Settings → Rules → Rulesets → New branch ruleset**, targeting `main`:

- Require a pull request before merging
- Require status checks: **build** (the Windows job)

This stops a broken build reaching the branch the installer is cut from. Skip it if you would
rather push straight to `main` while you are the only one working on it.

---

## Still to decide: a licence

The repository has no `LICENSE` file, which legally means all rights reserved — people can read
the code but not use or fork it. That is a real decision rather than a default, and it interacts
with the bundled ffmpeg (see [THIRD-PARTY.md](../THIRD-PARTY.md)):

- **GPL-3.0** is the coherent choice if you keep shipping the GPL ffmpeg build. It is also what
  most comparable editors use.
- **MIT or Apache-2.0** are the permissive options, but then the bundled ffmpeg should be
  switched to the LGPL build, which loses the x264 software encoder.

Add the file through **Add file → Create new file → `LICENSE`**; GitHub offers a template picker.
