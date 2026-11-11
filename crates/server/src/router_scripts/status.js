// Is the Wi-Fi on? Return true, false, or null when unsure. Netgear Nighthawk RS200v2 (adapted from the NetgearWiFi{On,Off}.js Scriptable scripts).
// Sets the MAIN 2.4 GHz + 5 GHz "Enable Wireless Router Radio" checkboxes on the Advanced
// Wireless page and leaves the 6 GHz radio alone.
//
// Available here: router.url, router.user, router.password (from Settings), http(), btoa(),
// atob(), sleep(ms), console.log(), RouterError(message, badPassword). Throw to fail; the
// message shows on the Wi-Fi card.

// The router's login page sends the password base64-encoded twice, with a random character
// on each end of the inner encoding.
function encodeTwice(pw) {
  const cs = "0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ=";
  const r = () => cs[Math.floor(Math.random() * cs.length)];
  return btoa(r() + btoa(pw) + r());
}

// Log in; the session cookie is kept for the rest of this run. The login page itself treats a
// reply of 3 as "another admin is signed in" and 5 as "refused".
async function login() {
  const r = await http({
    method: "POST",
    url: "/apply.cgi?%20timestamp=undefined",
    form: { submit_flag: "admin_login", username: router.user, password: encodeTwice(router.password) },
  });
  const reply = r.body.trim();
  if (reply === "3") throw new Error("another admin session is open on the router (its web page or the Nighthawk app)");
  if (reply === "5") throw new RouterError("the router refused the login (wrong password, or locked out after too many tries)", true);
}

// The router allows one admin session at a time, so always end ours.
async function logout() {
  try { await http({ url: "/LGO_logout.htm", timeout: 5 }); } catch (e) { /* best effort */ }
}

// The Advanced Wireless page: its one-time form token and the radios' current settings.
async function advancedWireless() {
  const page = await http({ url: "/WLG_adv.htm" });
  const html = page.body;
  const m = html.match(/\/apply\.cgi\?\/WLG_adv\.htm(?: |%20)timestamp=(\d+)/);
  if (!m) {
    // Without the token the router answers HTTP 400, so don't send a change.
    if (/admin_login|type=["']?password/i.test(html))
      throw new RouterError("the router refused the login (wrong password, or locked out after too many tries)", true);
    throw new Error("the router didn't return its settings page (HTTP " + page.status + ")");
  }
  const radio = (name) => (html.match(new RegExp("var " + name + "\\s*=\\s*['\"](\\d)['\"]")) || [])[1];
  return { token: m[1], wl: radio("con_endis_wl_radio"), wla: radio("con_endis_wla_radio") };
}

await login();
try {
  const page = await advancedWireless();
  if (page.wl === "1" && page.wla === "1") return true;
  if (page.wl === "0" && page.wla === "0") return false;
  console.log("2.4 GHz: " + page.wl + ", 5 GHz: " + page.wla);
  return null;
} finally {
  await logout();
}
