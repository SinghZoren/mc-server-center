# Code signing policy

Free code signing provided by [SignPath.io](https://about.signpath.io), certificate by
[SignPath Foundation](https://signpath.org).

## What is signed

The Windows files of each tagged release:

- `MCServerCenter-Setup-<version>-x64.exe` (the installer)
- `MC Server Center.exe` (`msm-desktop.exe`), `msmd.exe` and `msmctl.exe`, both inside the
  installer and in the portable zip

The Linux files are not Authenticode-signed; check them against `SHA256SUMS` on the release.

Everything that is signed is built from this repository's source by GitHub Actions
([`.github/workflows/release.yml`](../.github/workflows/release.yml)) on a pushed version tag.
Nothing built on a personal computer is ever signed.

## Team roles

| Role | Members |
|---|---|
| Committers and reviewers | [SinghZoren](https://github.com/SinghZoren) |
| Approvers (approve each signing request) | [SinghZoren](https://github.com/SinghZoren) |

All members use multi-factor authentication for GitHub and SignPath.

## Privacy

This program does not send any information to other networked systems unless the user asks it to,
and it has no telemetry. It contacts only:

- **Mojang, PaperMC, Fabric** (`*.mojang.com`, `*.papermc.io`, `*.fabricmc.net`) to download the
  Minecraft server software you choose.
- **Eclipse Adoptium** (`api.adoptium.net`, `github.com`) to download Java when a server needs it.
- **Modrinth** (`api.modrinth.com`, `cdn.modrinth.com`) when you search for or install mods and
  plugins.
- **Mojang** (`api.mojang.com`) to look up a player's name when you add them to the whitelist or
  ops.
- **ipify** (`api.ipify.org`), **mcsrvstat.us** (`api.mcsrvstat.us`) and **Cloudflare DNS**
  (`cloudflare-dns.com`) only when you run the connection check or the domain check.
- **Cloudflare** (`api.cloudflare.com`), with your own API token, only if you link a domain
  through Cloudflare.
- Computers you pair with yourself (your own VPS or Linux server, for remote management or the
  relay).

---

## Setting up signing (maintainers)

The release workflow signs automatically once these are in place. Until then, releases are
built unsigned.

1. **Apply** at [signpath.org](https://signpath.org) for the open-source program with this
   repository. Once accepted, SignPath creates the organization and a project for it.
2. **In SignPath**, open the project and check:
   - **Trusted build system:** GitHub.com is linked to the project (they set this up; it makes
     SignPath verify that each file was built by this repository's GitHub Actions).
   - **Artifact configuration:** set the project's *default* configuration to the one below. It
     signs every `.exe` in the uploaded artifact, which covers both signing requests of a release
     (the three programs, then the installer).
   - **Signing policy:** note its slug (usually `release-signing`).
3. **Create an API token** for a CI user in SignPath (*Users → Add CI user*, give it the
   *Submitter* role on the project) and copy the token.
4. **In GitHub**, open the repository's *Settings → Secrets and variables → Actions*:
   - *Secrets* → **New repository secret**: `SIGNPATH_API_TOKEN` = the token.
   - *Variables* → **New repository variable**: `SIGNPATH_ORGANIZATION_ID` = the organization ID
     (shown in SignPath's organization settings).
   - Only if they differ from the defaults: `SIGNPATH_PROJECT_SLUG` (default
     `mc-server-center`) and `SIGNPATH_SIGNING_POLICY_SLUG` (default `release-signing`).
5. **Release** as usual (bump the version, push a `v…` tag). The workflow pauses twice and
   waits for you to **approve** the signing request on signpath.io (up to an hour each): first
   for the programs, then for the installer.

Default artifact configuration:

```xml
<?xml version="1.0" encoding="utf-8"?>
<artifact-configuration xmlns="http://signpath.io/artifact-configuration/v1">
  <zip-file>
    <pe-file-set>
      <include path="*.exe" min-matches="1" max-matches="3" />
      <for-each>
        <authenticode-sign />
      </for-each>
    </pe-file-set>
  </zip-file>
</artifact-configuration>
```

Signing builds reputation with Windows SmartScreen under the SignPath Foundation certificate. A
brand-new release may still show the SmartScreen notice until enough people have downloaded it.
