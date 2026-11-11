// Turn the Wi-Fi ON. Netgear Nighthawk RS200v2 (adapted from the NetgearWiFi{On,Off}.js Scriptable scripts).
// Sets the MAIN 2.4 GHz + 5 GHz "Enable Wireless Router Radio" checkboxes on the Advanced
// Wireless page and leaves the 6 GHz radio alone.
//
// Available here: router.url, router.user, router.password (from Settings), http(), btoa(),
// atob(), sleep(ms), console.log(), RouterError(message, badPassword). Throw to fail; the
// message shows on the Wi-Fi card.

const WANT = "on";

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

// The full "Advanced Wireless" form as captured from the router. Only wl_enable_router /
// wla_enable_router (plus enable_ap / enable_ap_an when turning on) change; everything else is
// sent back unchanged.
const FORM = {
  submit_flag: "wlan_adv",
  wl_rts: "2347",
  wl_frag: "2346",
  wl_enable_shortpreamble: "0",
  wl_tx_ctrl: "100",
  wl_enable_router: "0",
  endis_pin: "0",
  hid_protect_enable: "1",
  hid_super_wifi: "0",
  hid_super_wifi_an: "0",
  wla_rts: "2347",
  wla_frag: "",
  wla_enable_shortpreamble: "0",
  wla_tx_ctrl: "100",
  wla_enable_router: "0",
  hid_super_wifi_tri: "0",
  wla_2nd_rts: "",
  wla_2nd_frag: "",
  wla_2nd_enable_shortpreamble: "",
  wla_2nd_tx_ctrl: "",
  wla_2nd_enable_router: "0",
  wla_6g_rts: "2347",
  wla_6g_frag: "",
  wla_6g_enable_shortpreamble: "0",
  wla_6g_enable_router: "1",
  wig_enable_router: "ignore",
  wds_change_ip: "still_lanip",
  enable_router: "ignore",
  enable_ssid_broadcast: "ignore",
  endis_wsc_config: "5",
  endis_wsc_config_a: "5",
  endis_wsc_config_tri: "1",
  wladv_endis_wmm: "1",
  wladv_endis_wmm_a: "1",
  wladv_endis_wmm_tri: "",
  wladv_endis_wmm_6g: "0",
  hid_dyn_get_ip: "",
  hid_ap_ipaddr: "",
  hid_ap_subnet: "",
  hid_ap_gateway: "",
  ap_dnsaddr1: "",
  ap_dnsaddr2: "",
  wladv_enable_schedule: "0",
  wladv_enable_schedule_a: "0",
  wladv_enable_schedule_tri: "0",
  wladv_enable_schedule_6g: "0",
  wladv_schedule_type: "",
  wladv_schedule_edit_num: "",
  wladv_schedule_delete_num: "",
  hid_enable_coexist: "0",
  hid_wlg_mu_mimo: "1",
  hid_wla_beamforming: "1",
  hid_wla_mu_mimo: "1",
  hid_wla_bf: "1",
  hid_enable_atf: "1",
  hid_wla_ht160: "1",
  hid_11k: "0",
  hid_disable_pmf: "0",
  hid_wla_2nd_beamforming: "",
  hid_wla_2nd_mu_mimo: "1",
  hid_wla_2nd_bf: "",
  hid_wla_2nd_ht160: "1",
  hid_wla_6g_mu_mimo: "1",
  hid_wla_6g_beamforming: "",
  hid_wla_6g_bf: "1",
  hid_wla_6g_ofdma: "1",
  hid_mlo_mode: "0",
  hid_mlo_change_reboot: "",
  enable_coexistence: "on",
  wmm_enable: "1",
  frag: "2346",
  rts: "2347",
  enable_shortpreamble: "automatic",
  tx_power_ctrl: "100",
  wmm_enable_a: "1",
  frag_an: "2346",
  rts_an: "2347",
  enable_shortpreamble_an: "automatic",
  tx_power_ctrl_an: "100",
  enable_ap_6g: "1",
  frag_6g: "2346",
  rts_6g: "2347",
  enable_short_preamble_6g: "automatic",
  tx_power_ctrl_6g: "100",
  pin_disable: "1",
  prevent_pin_compromise: "on",
  pin_attack_count: "3",
  wsc_config: "on",
  wla_wsc_config: "on",
  enable_mu: "",
  enable_atf: "on",
  enable_ht160: "",
};

const enable = WANT === "on" ? "1" : "0";
await login();
try {
  const page = await advancedWireless();
  // Applying restarts the radios, so leave them alone if they're already set.
  if (page.wl === enable && page.wla === enable) {
    console.log("Wi-Fi is already " + WANT);
    return;
  }
  const body = Object.assign({}, FORM, { wl_enable_router: enable, wla_enable_router: enable });
  if (WANT === "on") { body.enable_ap = "1"; body.enable_ap_an = "1"; }
  const r = await http({
    method: "POST",
    url: "/apply.cgi?/WLG_adv.htm%20timestamp=" + page.token,
    headers: { Referer: router.url + "/WLG_adv.htm" },
    form: body,
    timeout: 20,
  });
  if (r.status >= 400) throw new Error("the router rejected the change (HTTP " + r.status + ")");
  console.log("Wi-Fi turned " + WANT);
} finally {
  await logout();
}
