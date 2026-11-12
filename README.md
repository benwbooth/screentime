# screentime

Screen time controls for Linux, in the spirit of Windows Family Safety: daily allowances,
downtime schedules, and kids asking for more time with a notification on your phone.

![Today view on desktop](docs/screenshots/desktop-today.jpg)

## Pieces

| Crate | What it does |
|---|---|
| `crates/server` | Axum + SQLite policy server over HTTPS, password protected. Hosts the web UI and sends Web Push (or ntfy) notifications with one-tap grant buttons. |
| `crates/web` | Leptos admin UI with desktop and mobile layouts: downtime schedules, allowances, bonus time (+0/15/30/45/60/custom minutes), pending requests. |
| `crates/client` | Root daemon on each machine. Tracks active time through logind, warns at 10/5/1 minutes, locks then terminates sessions, caches policy for offline use. |
| `crates/pam` | `pam_screentime.so`, an account-phase module that blocks login outside allowed time by asking the local daemon. |
| `crates/greeter` | greetd greeter with a "Request more time" button. |
| `crates/windows` | Windows client: a service (same agent as Linux) and a tray app with the warning toast. |
| `crates/common` | Shared types. |

## Downloads

Each [release](https://github.com/benwbooth/screentime/releases) has prebuilt builds:
`screentime-linux-x86_64.tar.gz` / `-aarch64` (server, client daemon, PAM module, greeter and web UI;
the server finds the web UI in `share/screentime/web` next to `bin`), `screentime-windows.zip`
(see below) and `screentime-macos.zip` (unzip, then run `install.sh` from it the same way as from a
checkout; the apps are already built). The server is also an image:
`docker pull ghcr.io/benwbooth/screentime:latest` (data in `/data`, port 8443).
Pushing a `v*` tag builds and publishes a release (`.github/workflows/release.yml`).

## Windows

Build the installer zip from Linux with `crates/windows/build-windows.sh` (MinGW from nixpkgs);
it lands in `target/windows-dist/screentime-windows.zip`. On the PC, unzip it and run from an
administrator PowerShell:

```powershell
powershell -ExecutionPolicy Bypass -File install.ps1 `
  -Server https://benwbooth-nixos.duckdns.org:8443 -Token <token> -Users chloe
```

Get the token from the web UI (Computers > Add) or `screentime-server add-client <hostname>`.
The kid's account must be a standard user. `uninstall.ps1` removes everything and resets their
sign-in hours.

- **Service** (`screentime-service.exe`, runs as SYSTEM, also in Safe Mode): counts time while
  the kid's session is unlocked, warns at 10/5/1 minutes, then disconnects the session to the
  sign-in screen and logs it off after the grace period. It writes the schedule into the account's
  **Windows logon hours**, so Windows itself refuses sign-in during downtime or once the day's time
  is used, even if the service isn't running.
- **Tray app** (`screentime-tray.exe`, started in the kid's session by the service): the warning
  toast in the lower-right corner (click-through, never takes focus), and a tray menu to ask for
  15/30/60 more minutes or for "My parent is here", which opens the Windows credential prompt for
  an administrator account. `screentime-tray.exe --preview` shows the toast.
- **Sign-in screen tile** (`screentime_credprov.dll`, a Credential Provider): a "Screen time"
  tile on the sign-in and lock screens that shows where each limited account stands, lets the kid
  ask for 15/30/60 more minutes, and lets a parent who is there add time with an administrator
  account and password. It never signs anyone in, and only appears when the service is running
  and the computer has limited accounts.
- **CLI** (`screentime.exe status|users|request|parent`) works over the service's named pipe.

## Building

`cargo build` uses your normal Cargo setup (kache, if `~/.cargo/config.toml` has it). The web UI
is built with `crates/web/build.sh`. `nix build` compiles in a persistent target directory,
`/var/tmp/cargo-nix/screentime` (and `-web`), when the sandbox exposes `/var/tmp/cargo-nix`
(`nix.settings.extra-sandbox-paths = [ "/var/tmp/cargo-nix?" ]`), so a rebuild only recompiles
what changed; otherwise it builds from scratch in the sandbox.

## NixOS

```nix
inputs.screentime.url = "github:benwbooth/screentime";

# on the server
imports = [ screentime.nixosModules.server ];
services.screentime.server.enable = true;

# on each client
imports = [ screentime.nixosModules.client ];
services.screentime.client = {
  enable = true;
  serverUrl = "https://screentime.example:8443";
  managedUsers = [ "kid" ];
  greeter.enable = true;
};
```

## Status

- **Client daemon:** working. Counts unlocked session time through logind, warns at 10/5/1 minutes
  to subscribers on its socket, locks sessions when time runs out and ends them after a grace period,
  caches policy in `/var/lib/screentime/state.json`, syncs with the server, queues time requests.
- **"My parent is here":** working in the daemon and the `screentime` command; a parent's
  admin password adds time on the spot, even during downtime or offline.
- **PAM module:** working. Fails closed for managed users and shows the reason at login.
- **NixOS client module:** working, with assertions against the boot menu editor and managed users in wheel.
- **Server:** working. SQLite storage, per-day allowances shared across machines, downtime,
  bonuses, time requests, per-machine tokens, admin login (argon2, HttpOnly SameSite=Strict cookie,
  throttled), HTTPS via rustls, and a private CA generator for self-signed setups.
- **Notifications:** built-in Web Push (no extra app or service) with one-tap +N buttons, plus a
  page offering 0/15/30/45/60/custom. ntfy is optional.
- **Web UI:** working. Leptos, phone layout (bottom tabs) and desktop layout (sidebar): today's
  status and requests, add time, per-day limits and downtime, computers, notifications.
- **Wi-Fi time:** working. Wi-Fi is a built-in user with a schedule, added time, a lock and
  history; when it isn't allowed, the server turns the Netgear router's Wi-Fi off.
- **Greeter, warning overlay:** not started.

## Setting up the server

```sh
screentime-server hash-password > admin.hash      # then point adminPasswordHashFile at it
screentime-server gen-cert /var/lib/screentime-tls screentime.lan 192.168.1.10
screentime-server add-client kids-pc              # prints the token for that machine's tokenFile
```

`gen-cert` makes a certificate for the names and addresses you list and prints its fingerprint.
Computers trust the server by that fingerprint, so there is nothing to install on them:

```nix
services.screentime.client.serverFingerprint = "1F:E7:E4:...:3E:6A";
```

The fingerprint changes each time you run `gen-cert`; the server also logs it at startup, and
`screentime-server fingerprint cert.pem` prints it.

**Easiest: a free DuckDNS name.** If you have one, the server can get a real Let's Encrypt
certificate through a DNS challenge, without being reachable from the internet. Phones and
computers then trust it with nothing to install or pin, and built-in notifications work:

```nix
services.screentime.server.duckdns = {
  domain = "yourname.duckdns.org";
  tokenFile = "/run/secrets/duckdns_token";   # just the token
  email = "you@example.com";
};
```

The name must lead to the server from wherever the phone is: a router DNS override (or NAT
loopback) at home and over the VPN, or a port forward.

**Phones are stricter.** Browsers only allow notifications from an HTTPS site they trust, and a
fingerprint can't be given to a phone browser. With a self-signed certificate the web UI works
after tapping through the warning, but notifications won't turn on. For notifications without
installing anything, use a certificate the phone already trusts:

- **Tailscale:** `tailscale cert yourserver.<tailnet>.ts.net`, then point `tls.certFile`/`keyFile` at it.
- **Your own domain:** Let's Encrypt with a DNS challenge through NixOS `security.acme`; the server
  doesn't need to be reachable from the internet.

Or install `ca.pem` from the `gen-cert` directory on the phone once (iPhone: install the profile,
then enable it under Settings > General > About > Certificate Trust Settings; Android: Settings >
Security > Install a certificate > CA certificate).

### Notifications

There are two ways to get time requests on your phone; you can use either or both.

**ntfy (works with a self-signed certificate).** Install the ntfy app, subscribe to a long random
topic such as `https://ntfy.sh/screentime-k3v9q2x7...`, and set
`services.screentime.server.ntfy.url` to it. Requests arrive with up to three +N buttons. A
button posts its answer to `<topic>-answers` on ntfy, which the server follows, so the phone never
connects to the server: the buttons work from anywhere and no certificate has to be trusted.
Each answer carries a single-use secret, so only buttons from a real notification can grant time.
Tapping the notification opens the page with every amount (0/15/30/45/60/custom); that needs the
server reachable, e.g. over your VPN. Keep the topic name secret: anyone who knows it can read the
notifications and use their buttons.

**Built-in Web Push (no app).** Open the web UI on your phone, go to Account and tap "Turn on
notifications". This needs a certificate the phone trusts (Let's Encrypt, or `ca.pem` installed
on the phone). On iPhone, first add the page to the Home Screen and open it from there.

```nix
services.screentime.server = {
  enable = true;
  openFirewall = true;
  publicUrl = "https://screentime.example.net:8443";
  tls = { certFile = "/var/lib/screentime-tls/cert.pem"; keyFile = "/var/lib/screentime-tls/key.pem"; };
  adminPasswordHashFile = "/run/secrets/screentime-admin-hash";
};
```

### Wi-Fi time

Wi-Fi is a built-in user, "Wi-Fi", with everything a kid has: a daily limit and downtime under
Schedules, time added or taken away and a lock on the Today page, and its own History. It
starts with no limit. Its minutes count while the Wi-Fi is on. When it's out of time, in
downtime or locked, the server switches the router's main 2.4 GHz and 5 GHz radios off (the
6 GHz radio is left alone), and back on when it's allowed again. Pausing Wi-Fi time stops the
switching and, if the server had switched the Wi-Fi off, turns it back on.

The router is switched by three JavaScript scripts, edited under Settings → Router scripts:
**Check** (returns whether the Wi-Fi is on; run when the server starts, so a restart doesn't
re-apply a setting that's already there), **Turn on** and **Turn off**. The built-in ones drive a
Netgear Nighthawk RS200 through its web admin pages (log in, read the Advanced Wireless page's
one-time token and radio settings, post the form, log out); replace them for another router.
Each has a Test button (Turn on/off ask first, since they change the real Wi-Fi).

Scripts run in an embedded QuickJS with a one-minute limit and no file access. They get
`router.url`, `router.user` and `router.password` (the password is filled in at run time, never
stored in the script), `await http({method, url, headers, form, body, timeout})` returning
`{status, headers, body}` (cookies kept for the run, a browser User-Agent, URLs starting with
`/` go to the router), `btoa`, `atob`, `await sleep(ms)`, `console.log` and
`RouterError(message, badPassword)`. A script fails by throwing; the message shows on the Wi-Fi
card. The router allows one admin session at a time, so scripts run one at a time and only when
the Wi-Fi needs to change. Failures are retried every minute (every 15 minutes after a
`RouterError` with `badPassword`, to avoid the router's lockout).

Set it up under Settings → Wi-Fi router: the router's address (default `http://192.168.1.1`),
username (`admin`) and password, then tick "Switch the Wi-Fi by its schedule and time". Nothing
touches the router until then. The password is stored in the server's database (with the rest
of its data, never in this repository) and is never sent back to the browser; the page only
shows whether one is saved. The server has to reach the router on the LAN.

See [docs/DESIGN.md](docs/DESIGN.md) for the anti-bypass and warning design.
