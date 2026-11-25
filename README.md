# Parts Relay tunnel

A small executable a parts seller installs on its own network. It connects out to Parts Relay on port 443 and relays connections to a management system that is not reachable from the internet.

It is only a proxy. It knows nothing about any management system: Parts Relay asks it for a TCP connection to `host:port`, and the tunnel opens it only if that exact `host:port` is in its `allow` list.

```
Parts Relay (connectors)  <== one outbound WebSocket, TLS, port 443 ==  tunnel  -- TCP -->  management system
```

- No inbound port to open: the tunnel dials out.
- Works behind a corporate HTTP proxy and with TLS inspection, trusting what the operating system trusts.
- One static executable for Linux, macOS and Windows, on x86_64 and arm64.
- Updates itself from the [releases](https://github.com/PartsRelay/tunnel/releases) of this repository, and only to a release signed with the key in [`minisign.pub`](minisign.pub).

The wire protocol is in [docs/protocol.md](docs/protocol.md).

## Install

Linux and macOS, as root, with the tunnel registered as a service:

```sh
curl -fsSL https://raw.githubusercontent.com/PartsRelay/tunnel/main/install.sh | sudo sh -s -- --service
```

Windows, in an administrator PowerShell:

```powershell
irm https://raw.githubusercontent.com/PartsRelay/tunnel/main/install.ps1 -OutFile install.ps1
.\install.ps1
```

Both scripts download the archive of the latest release for the computer, check its SHA-256 against the release manifest and install:

|                | Linux and macOS                        | Windows                                             |
| -------------- | -------------------------------------- | --------------------------------------------------- |
| Executable     | `/opt/partsrelay/bin/partsrelay-tunnel` | `C:\Program Files\PartsRelay\partsrelay-tunnel.exe` |
| Configuration  | `/etc/partsrelay/tunnel.toml`          | `C:\ProgramData\PartsRelay\tunnel.toml`             |
| Service        | systemd unit, or launchd daemon        | scheduled task `PartsRelay Tunnel`                  |

You can also download an archive from the releases page and run the executable from anywhere.

The install scripts trust GitHub over TLS. To also check the signature of the release at install time, download [`minisign.pub`](minisign.pub), install [minisign](https://jedisct1.github.io/minisign/) and pass `--pubkey minisign.pub` to `install.sh`. Once installed, the tunnel checks the signature of every update itself.

## Configure

Set the token Parts Relay gave you and the targets the tunnel may reach, then start the service.

```toml
# /etc/partsrelay/tunnel.toml

token = "prt_..."

# Only these are ever dialed, exactly as written
allow = ["192.168.1.10:8080", "erp.local:1433"]
```

| Setting       | Environment variable            | Default                                     | Meaning                                                                        |
| ------------- | ------------------------------- | ------------------------------------------- | ------------------------------------------------------------------------------ |
| `token`       | `PARTSRELAY_TUNNEL_TOKEN`       | none, required                              | The token of this tunnel                                                       |
| `allow`       | `PARTSRELAY_TUNNEL_ALLOW`       | empty: everything is refused                | Targets as `host:port`. In the variable, separated by commas                   |
| `server`      | `PARTSRELAY_TUNNEL_SERVER`      | `wss://connectors.partsrelay.com/v1/tunnel` | Where the tunnel connects. `ws://` is accepted only for this computer          |
| `auto_update` | `PARTSRELAY_TUNNEL_AUTO_UPDATE` | `true`                                      | Install signed releases by itself                                              |
| `proxy`       | `PARTSRELAY_TUNNEL_PROXY`       | `HTTPS_PROXY`, then `ALL_PROXY`             | An `http://` proxy, with `user:password@` if it asks for them                  |
| `ca_file`     | `PARTSRELAY_TUNNEL_CA_FILE`     | none                                        | PEM certificates to trust besides the ones of the operating system             |
| `max_streams` | `PARTSRELAY_TUNNEL_MAX_STREAMS` | `64`                                        | Connections relayed at once, 1 to 1024                                         |

A variable wins over the file, and the file is optional when the variables say everything. `--config FILE` (or `PARTSRELAY_TUNNEL_CONFIG`) reads another file. An unknown setting is an error, so a typo never goes unnoticed.

The allow list compares exactly: a name without its case and trailing dot, an address as an address. A name does not allow its address, and there are no wildcards. Prefer addresses when the DNS of the network is not under your control.

## Run

```sh
partsrelay-tunnel            # connect and relay, the same as "run"
partsrelay-tunnel check      # validate the configuration and try every connection
partsrelay-tunnel update     # install the latest release now
partsrelay-tunnel update --check
partsrelay-tunnel --version
```

`check` tells whether each target of the allow list answers and whether Parts Relay accepts the token. Run it first when something does not work.

Logs go to standard error. `--log-level debug` shows every relayed connection, `--log-format json` writes one JSON object per line. The token and the proxy password are never logged.

### As a service

`install.sh --service` and `install.ps1` register the service but do not start it, so you can write the configuration first.

| System  | Start                                                                      | Logs                                          |
| ------- | -------------------------------------------------------------------------- | --------------------------------------------- |
| Linux   | `systemctl start partsrelay-tunnel`                                        | `journalctl -u partsrelay-tunnel -f`          |
| macOS   | `launchctl bootstrap system /Library/LaunchDaemons/com.partsrelay.tunnel.plist` | `/var/log/partsrelay-tunnel.log`              |
| Windows | `Start-ScheduledTask -TaskName 'PartsRelay Tunnel'`                        | none by default, run it by hand to see them   |

On Linux the service runs as the unprivileged user `partsrelay`, with a read only system. On Windows it runs as `LOCAL SERVICE`. On macOS it runs as root in this version.

A broken configuration ends the tunnel with exit code 2 and systemd does not restart it. Any other end is restarted.

## Proxies and certificates

- The tunnel uses the proxy of `proxy`, else of `HTTPS_PROXY`, else of `ALL_PROXY`, and honors `NO_PROXY` for the proxies found in the environment. A service usually has no such variables: set `proxy` in the file.
- Only `http://` proxies with no or Basic authentication are supported. NTLM, Kerberos, SOCKS and PAC files are not.
- TLS is verified with the certificate store of the operating system, so the certificate authority of a TLS inspecting proxy works once it is installed there. `ca_file` adds certificates for the connection to Parts Relay only. There is no option to skip verification.
- Let these hosts through: the host of `server` on port 443, and for the updates `github.com` and `release-assets.githubusercontent.com` on port 443.

## Updates

A few seconds after it starts, and then about every 6 hours, the tunnel:

1. downloads `manifest.json` and `manifest.json.minisig` of the latest release;
2. verifies the signature with the public key built into the executable. Without a valid signature nothing else happens;
3. goes on only if the version is higher than its own. It never downgrades;
4. downloads the archive of its platform and checks its size and SHA-256 against the manifest;
5. runs the new executable once with `--version`;
6. puts it in place, keeps the previous one next to it as `partsrelay-tunnel.old`, lets the open connections finish for up to 30 seconds and restarts.

On Linux and macOS it restarts by replacing its own process. On Windows it exits and the scheduled task starts the new version within a minute.

If the directory of the executable is read only, the tunnel says so once and keeps running without updates. `auto_update = false` turns them off, and `partsrelay-tunnel update` installs one by hand.

## Security model

What the tunnel protects, and what it does not.

- **Parts Relay can only reach what you allow.** The allow list lives in your configuration file. Nothing sent by the server changes it, and the server cannot run commands or push an update: updates come only from the signed releases of this repository.
- **A target that is itself a proxy widens the list.** If you allow a host that forwards connections elsewhere, Parts Relay reaches what that host reaches.
- **The network cannot read or change the traffic**, unless it is a TLS inspecting proxy whose certificate authority you installed: that proxy sees the token and the relayed bytes.
- **A stolen release cannot be installed.** Whoever replaces the files of a release without the signing key fails the signature check. They can at most stop the updates.
- **A local administrator owns the tunnel.** Whoever can read the configuration file has the token, and whoever can write the executable controls it. The install scripts restrict both to the service account and the administrators.
- Limits that bound a misbehaving server: 64 connections at once by default, 4 KiB for the request of a connection, 10 seconds to send it and 10 seconds to connect, WebSocket messages up to 1 MiB.

Report a vulnerability privately through the security advisories of this repository.

## Build from source

```sh
cargo build --release
cargo test
```

Rust 1.97 is pinned in `rust-toolchain.toml`. The crate forbids `unsafe` code and uses `ring` for cryptography, so the only native dependency is a C compiler.

## For maintainers

### Releases

Commits on `main` follow [Conventional Commits](https://www.conventionalcommits.org), and the title of every pull request is checked for it. [release-please](https://github.com/googleapis/release-please) reads them:

1. on every push to `main` it opens or updates a release pull request with the next version and the changelog;
2. merging that pull request creates the tag and a draft release;
3. the same workflow builds the six executables, writes `manifest.json`, signs it, uploads everything and only then publishes the release. So the latest release always has its files.

If a build fails, fix it and run the `Release` workflow by hand with the tag of the draft.

`feat` and `fix` commits make a release. `build` and `ci` commits, like the ones of Dependabot, do not: retitle a dependency update to `fix(deps): ...` when it must reach the installed tunnels.

### One time setup

1. **Signing key.** The `minisign.pub` in this repository is a placeholder whose secret half was thrown away. Until you replace it, the release workflow fails at its verify step and no installed tunnel can update.

   ```sh
   minisign -G -W -p minisign.pub -s minisign.key
   ```

   Commit the new `minisign.pub`. Add the whole content of `minisign.key` as the secret `MINISIGN_SECRET_KEY`, in the `release` environment of the repository or among its secrets, then keep the file offline or delete it.

2. **Repository settings.** In Settings, Actions, General: allow GitHub Actions to create and approve pull requests. Merge with squash, with the pull request title as the commit message. Protect `main` and restrict the `release` environment to it.

3. **Optional, `RELEASE_PLEASE_TOKEN`.** Pull requests opened with the default token of Actions do not start other workflows, so the release pull request shows no checks. Its changes are only the version and the changelog. If you make the checks required, add a fine grained token or a GitHub App token as the secret `RELEASE_PLEASE_TOKEN`: the workflow uses it when present.

### Rotating the key

An executable trusts the key it was built with, so a tunnel can only move to a release signed with a key it already knows. To change key, the code first has to accept both: `update::manifest::public_keys` returns a list for this. Ship one release that trusts the old and the new key, signed with the old one, and sign the following releases with the new one.

### Not verified yet

The scheduled task of `install.ps1` and the Windows update path were written against the documentation and need one pass on a real Windows computer.

## License

[MIT](LICENSE)
