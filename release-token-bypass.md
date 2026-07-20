# Letting the release workflow push to a protected `main`

`.github/workflows/release.yml` pushes the release commit (the version bump +
promoted `CHANGELOG.md`) and the `v<version>` tag straight to `main`. Once you
protect `main` with a rule that **requires pull requests**, that direct push is
rejected — for every actor except those on the rule's **bypass list**.

You cannot put the built-in `github-actions[bot]` on a bypass list (it is a
system actor, not an addressable App), and a personal access token expires and
ties the push to a human. The supported path is a **GitHub App**: the workflow
mints a short-lived installation token (auto-revoked, no rotation), pushes as the
App, and the App sits in the ruleset's bypass list.

When `main` is **not** protected, none of this is needed — the workflow falls
back to the default `GITHUB_TOKEN` and the push just works. Set this up only once
you turn on PR-required branch protection.

## One-time setup

1. **Create a GitHub App** (Settings → Developer settings → GitHub Apps → *New
   GitHub App*). Minimal config:
   - **Repository permissions → Contents: Read and write** (to push the commit +
     tag). Nothing else is required.
   - No webhook needed (uncheck *Active*).
   - It can be private to your account/org; it does not need to be public.

2. **Generate a private key** for the App (App settings → *Private keys* →
   *Generate a private key*) and download the `.pem`.

3. **Install the App** on the target repository (App settings → *Install App* →
   pick the repo).

4. **Add the credentials to the repo** (repo Settings → *Secrets and variables*
   → *Actions*):
   - **Variable** `RELEASE_APP_ID` = the App's numeric *App ID*.
   - **Secret** `RELEASE_APP_PRIVATE_KEY` = the full contents of the `.pem`
     (including the `-----BEGIN/END-----` lines).

   The workflow's "Mint GitHub App token" step is guarded by
   `if: ${{ vars.RELEASE_APP_ID != '' }}`, so until the variable exists the step
   is skipped and the push uses the default token.

5. **Add the App to the branch-protection bypass list.** Use a **repository
   ruleset** (repo Settings → *Rules* → *Rulesets*), which — unlike the older
   "branch protection rules" screen — supports a bypass list:
   - Target branch `main`, enable *Require a pull request before merging*.
   - Under **Bypass list**, add your App (it appears once installed).

   The App can now push directly to `main`; everyone else still goes through a
   PR.

## Verifying

Dispatch the release workflow (Actions → *Release* → *Run workflow* → pick a
bump). The **Mint GitHub App token** step should run (not skip), and the **Tag
and push** step should push the `Release v<version>` commit and tag to `main`
without a protection error. If the push is rejected, re-check that the App is
installed on the repo and is actually listed in the ruleset's bypass list, and
that `RELEASE_APP_ID` / `RELEASE_APP_PRIVATE_KEY` are set on the repo (not the
org, unless the App is org-owned).

> This is ordinary setup documentation: it applies only if `main` is protected by
> a ruleset that would otherwise reject the release workflow's tag push.
