# Branches

GitHub cannot hide one branch in a public repository. Scalattice Agent uses **two repos**:

| Repo | Visibility | Default branch | Role |
|------|------------|----------------|------|
| [scalattice/scalattice-agent](https://github.com/scalattice/scalattice-agent) | Public | `production` | Releases, source the world clones |
| [scalattice/scalattice-agent-dev](https://github.com/scalattice/scalattice-agent-dev) | Private | `development` | Day-to-day work |

WIP commits stay on the private repo. Promote **squash-pushes** the current tree onto public `production` (one public commit, no private history). That push runs the **Release** workflow (Linux x86_64 + aarch64 + Windows + macOS).

## Daily work

```bash
git clone git@github.com:scalattice/scalattice-agent-dev.git
cd scalattice-agent-dev
git checkout development
```

Open a PR **into `production` on the private repo** (or run **Promote to production** from Actions). Merging `development` → `production` there publishes to the public repo and ships a release.

Local shortcut (uses your `gh` login):

```bash
./scripts/promote-to-production.sh
```

## What not to do

- Do not push feature work to the public repo
- Do not put `development` on the public repo (it would be visible to everyone)
- Skip a production release with `[skip release]` in the public commit message

Manual rebuild of an existing tag (public repo Actions → Release):

```bash
gh workflow run release.yml -R scalattice/scalattice-agent \
  --ref production \
  -f tag=v1.1.33 -f targets=full -f windows_runner=self-hosted
```
