//! Payment page HTML renderer for the L402 402-challenge response.
//!
//! Owns all HTML, CSS and JavaScript sent to the browser when a protected
//! resource requires payment. It touches no nginx types, so it lives here
//! rather than in the `cdylib` — where `test = false` means a `#[test]` block
//! could never run, and the escaping and auto-detect wiring would go
//! unverified.

use crate::escaping::html_escape;

/// Serialise a value as a JSON literal safe to inline in a `<script>` block.
///
/// `serde_json` alone is not enough here: it leaves `<` intact, so a value
/// containing `</script>` would close the element early (XSS), and it emits
/// U+2028/U+2029 raw, which are line terminators to pre-ES2019 parsers and
/// break the script. Every inline-script interpolation goes through this so the
/// escaping cannot drift between call sites.
fn script_json_literal(s: &str) -> String {
    serde_json::to_string(s)
        .unwrap_or_else(|_| "\"\"".to_string())
        .replace('<', "\\u003c")
        .replace('>', "\\u003e")
        .replace('&', "\\u0026")
        .replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029")
}

/// Render the full 402 payment page as an HTML string.
///
/// # Arguments
/// * `invoice`               - BOLT-11 invoice string (used for QR + copy)
/// * `amount_msat`           - Amount in millisatoshis (displayed as sats)
/// * `macaroon_b64`          - Base64-encoded macaroon token
/// * `auto_detect`           - Whether to poll for automatic payment detection
/// * `cashu_enabled`         - Whether to show the Cashu/eCash tab
/// * `cashu_payment_request` - Optional P2PK Cashu payment request string
/// * `cashu_mints`           - Mints a token may be drawn from; empty means any
pub fn render_payment_page(
    invoice: &str,
    amount_msat: i64,
    macaroon_b64: &str,
    auto_detect: bool,
    cashu_enabled: bool,
    cashu_payment_request: Option<&str>,
    cashu_mints: &[String],
) -> String {
    // ── QR Code ─────────────────────────────────────────────────────────────
    // Generate at 280 px and inject centering styles into the SVG root so the
    // image always fills its white card wrapper without cropping.
    let raw_qr = qrcode_generator::to_svg_to_string(
        invoice.to_uppercase(),
        qrcode_generator::QrCodeEcc::Medium,
        280,
        None::<&str>,
    )
    .unwrap_or_else(|_| {
        "<svg xmlns='http://www.w3.org/2000/svg' width='280' height='280'>\
         <rect width='280' height='280' fill='#1a1a2e'/>\
         <text x='140' y='140' text-anchor='middle' fill='#8b5cf6' font-size='12'>QR Error</text>\
         </svg>"
            .to_string()
    });

    let qr_svg = raw_qr.replacen(
        "<svg ",
        "<svg viewBox=\"0 0 280 280\" style=\"display:block;width:100%;height:auto;max-width:280px;\" ",
        1,
    );

    // ── Amounts ──────────────────────────────────────────────────────────────
    let amount_sats = amount_msat / 1000;
    let invoice_short = if invoice.chars().count() > 40 {
        let head: String = invoice.chars().take(20).collect();
        let tail: String = invoice
            .chars()
            .rev()
            .take(10)
            .collect::<String>()
            .chars()
            .rev()
            .collect();
        html_escape(&format!("{}\u{2026}{}", head, tail))
    } else {
        html_escape(invoice)
    };

    // ── Cashu tab ────────────────────────────────────────────────────────────
    let cashu_tab_html = if cashu_enabled {
        // The full value is carried alongside the preview so the copy handler
        // has something to read: a `creq` string is a few hundred characters.
        let payment_req_hint = cashu_payment_request
            .map(|r| {
                let short: String = r.chars().take(48).collect();
                format!(
                    "<div class=\"payment-req-box\" onclick=\"copyPaymentReq()\" \
title=\"Click to copy the full payment request\">\
<span class=\"payment-req-label\">Payment Request \u{2014} click to copy</span>\
<code class=\"payment-req-code\">{short}\u{2026}</code>\
<span id=\"cashu-preq-full\" hidden>{full}</span>\
</div>\
<div class=\"copy-toast\" id=\"preq-toast\">Copied to clipboard!</div>",
                    short = html_escape(&short),
                    full = html_escape(r),
                )
            })
            .unwrap_or_default();

        // Hosts only — a full URL wraps badly on a phone.
        let mints_hint = if cashu_mints.is_empty() {
            String::new()
        } else {
            let items = cashu_mints
                .iter()
                .map(|m| {
                    let host = m
                        .trim()
                        .trim_start_matches("https://")
                        .trim_start_matches("http://")
                        .trim_end_matches('/');
                    format!("<li>{}</li>", html_escape(host))
                })
                .collect::<Vec<_>>()
                .join("");
            format!(
                "<div class=\"mints-box\">\
<span class=\"mints-label\">Tokens accepted from these mints only</span>\
<ul class=\"mints-list\">{items}</ul>\
</div>",
                items = items,
            )
        };
        format!(
            "<div id=\"tab-ecash\" class=\"tab-panel hidden\">\
<div class=\"card\">\
<div class=\"cashu-header\">\
<span class=\"cashu-icon\">🥜</span>\
<div>\
<div class=\"cashu-title\">Pay with Cashu eCash</div>\
<div class=\"cashu-subtitle\">Paste a Cashu token to instantly unlock access</div>\
</div>\
</div>\
{payment_req_hint}\
{mints_hint}\
<div class=\"field\">\
<label for=\"cashu-token\">Cashu Token</label>\
<textarea id=\"cashu-token\" placeholder=\"cashuA...\" rows=\"4\" spellcheck=\"false\" autocomplete=\"off\"></textarea>\
</div>\
<button class=\"btn btn-cashu\" onclick=\"submitCashu()\">🥜 Submit Token</button>\
<div id=\"cashu-error\" class=\"error-msg hidden\"></div>\
</div>\
</div>",
            payment_req_hint = payment_req_hint,
            mints_hint = mints_hint,
        )
    } else {
        String::new()
    };

    let ecash_tab_btn = if cashu_enabled {
        "<button class=\"tab-btn\" id=\"tab-btn-ecash\" onclick=\"switchTab('ecash')\">🥜 ECASH</button>"
    } else {
        ""
    };

    // ── Auto-detect polling JS ────────────────────────────────────────────────
    let auto_detect_js = if auto_detect {
        format!(
            r#"
    let pollAttempts = 0;
    const MAX_POLL = 100;
    function startPolling() {{
        if (pollAttempts++ > MAX_POLL) {{
            document.getElementById('auto-status').classList.add('hidden');
            document.getElementById('preimage-section').classList.remove('hidden');
            return;
        }}
        fetch(window.location.href, {{
            headers: {{'Authorization': 'L402 ' + {mac}}},
            redirect: 'follow',
            credentials: 'same-origin'
        }}).then(r => {{
            if (r.ok || r.status === 200) {{
                document.getElementById('auto-status').innerHTML =
                    '<span style="color:var(--success)">✓ Payment confirmed! Loading…</span>';
                setTimeout(() => showContent(r), 800);
            }} else {{
                setTimeout(startPolling, 3000);
            }}
        }}).catch(() => setTimeout(startPolling, 3000));
    }}
    startPolling();
"#,
            mac = script_json_literal(macaroon_b64)
        )
    } else {
        String::new()
    };

    let auto_detect_section = if auto_detect {
        "<div id=\"auto-status\" class=\"auto-status\">\
<div class=\"spinner\"></div>\
<span>Waiting for payment confirmation\u{2026}</span>\
<button class=\"btn-link\" onclick=\"document.getElementById('auto-status').classList.add('hidden');\
document.getElementById('preimage-section').classList.remove('hidden')\">Enter preimage manually</button>\
</div>"
    } else {
        ""
    };

    let preimage_hidden_class = if auto_detect { "hidden" } else { "" };

    // ── Full HTML page ────────────────────────────────────────────────────────
    format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8">
<meta name="viewport" content="width=device-width, initial-scale=1.0">
<title>402 Payment Required &#8212; L402</title>
<style>
  @import url('https://fonts.googleapis.com/css2?family=Inter:wght@300;400;500;600;700&family=JetBrains+Mono:wght@400;500&display=swap');
  *,*::before,*::after{{box-sizing:border-box;margin:0;padding:0}}
  :root{{
    --bg:#07070f;--surface:#0d0d1a;--surface2:#13132a;--border:#1e1e3a;
    --accent:#7c3aed;--accent2:#a855f7;--accent-glow:rgba(124,58,237,.35);
    --cashu:#f59e0b;--cashu-glow:rgba(245,158,11,.25);
    --text:#e2e2f0;--text-muted:#8080a8;--text-dim:#3a3a5c;
    --success:#10b981;--error:#ef4444;
  }}
  html,body{{height:100%;background:var(--bg);color:var(--text);font-family:'Inter',sans-serif;font-size:15px;line-height:1.6}}
  body{{
    display:flex;align-items:flex-start;justify-content:center;min-height:100vh;
    padding:2rem 1.5rem;overflow-y:auto;
    background-image:
      radial-gradient(ellipse 90% 70% at 50% -10%, rgba(124,58,237,.22), transparent),
      radial-gradient(ellipse 60% 50% at 85% 90%, rgba(168,85,247,.1), transparent),
      radial-gradient(ellipse 40% 30% at 10% 80%, rgba(124,58,237,.06), transparent);
  }}
  .container{{width:100%;max-width:460px;display:flex;flex-direction:column;gap:1.2rem}}
  /* Header */
  .header{{text-align:center;padding-bottom:.25rem}}
  .badge{{display:inline-flex;align-items:center;gap:.4rem;background:rgba(239,68,68,.1);border:1px solid rgba(239,68,68,.28);border-radius:9999px;padding:.28rem .85rem;font-size:.72rem;font-weight:700;letter-spacing:.1em;color:#f87171;margin-bottom:.85rem;text-transform:uppercase}}
  .header h1{{font-size:1.65rem;font-weight:700;letter-spacing:-.025em;background:linear-gradient(135deg,#e2e2f0 0%,#c084fc 60%,#a855f7 100%);-webkit-background-clip:text;-webkit-text-fill-color:transparent;background-clip:text;margin-bottom:.35rem}}
  .header p{{color:var(--text-muted);font-size:.88rem}}
  .amount{{display:inline-flex;align-items:baseline;gap:.35rem;margin-top:.6rem;background:linear-gradient(135deg,rgba(124,58,237,.12),rgba(168,85,247,.06));border:1px solid rgba(124,58,237,.28);border-radius:.8rem;padding:.45rem 1.1rem;box-shadow:0 0 20px rgba(124,58,237,.08)}}
  .amount-num{{font-size:1.55rem;font-weight:700;color:#c084fc}}
  .amount-unit{{font-size:.78rem;color:var(--text-muted);font-weight:500}}
  /* Tabs */
  .tabs{{display:flex;background:rgba(13,13,26,.9);border:1px solid var(--border);border-radius:.85rem;padding:.3rem;gap:.3rem;box-shadow:0 4px 24px rgba(0,0,0,.35)}}
  .tab-btn{{flex:1;padding:.52rem .5rem;border:none;border-radius:.55rem;background:transparent;color:var(--text-muted);font-size:.76rem;font-weight:600;letter-spacing:.05em;cursor:pointer;transition:all .22s;display:flex;align-items:center;justify-content:center;gap:.35rem}}
  .tab-btn.active{{background:var(--accent);color:#fff;box-shadow:0 0 18px var(--accent-glow)}}
  .tab-btn:hover:not(.active){{background:var(--surface2);color:var(--text)}}
  /* Cards */
  .card{{background:rgba(13,13,26,.85);border:1px solid var(--border);border-radius:1.1rem;padding:1.35rem;backdrop-filter:blur(16px);display:flex;flex-direction:column;gap:1.1rem;box-shadow:0 8px 40px rgba(0,0,0,.4)}}
  .tab-panel{{display:flex;flex-direction:column;gap:1rem}}
  .tab-panel.hidden{{display:none}}
  /* QR — white card with overflow:hidden so the SVG never bleeds */
  .qr-wrap{{display:flex;align-items:center;justify-content:center;background:linear-gradient(145deg,rgba(255,255,255,.97),rgba(248,248,255,.95));border-radius:.85rem;padding:1.1rem;overflow:hidden;box-shadow:0 2px 24px rgba(0,0,0,.45),inset 0 1px 0 rgba(255,255,255,.8)}}
  /* Invoice strip */
  .invoice-box{{background:var(--surface2);border:1px solid var(--border);border-radius:.65rem;padding:.7rem .9rem;display:flex;align-items:center;gap:.65rem;cursor:pointer;transition:border-color .2s,background .2s}}
  .invoice-box:hover{{border-color:var(--accent2);background:rgba(124,58,237,.06)}}
  .invoice-text{{font-family:'JetBrains Mono',monospace;font-size:.7rem;color:var(--text-muted);flex:1;word-break:break-all;overflow:hidden;display:-webkit-box;-webkit-line-clamp:2;-webkit-box-orient:vertical}}
  .copy-icon{{flex-shrink:0;color:var(--text-dim);font-size:1rem;transition:color .2s}}
  .invoice-box:hover .copy-icon{{color:var(--accent2)}}
  .copy-toast{{font-size:.72rem;color:var(--success);text-align:center;opacity:0;transition:opacity .3s;height:1rem}}
  /* Auto-detect */
  .auto-status{{display:flex;flex-direction:column;align-items:center;gap:.65rem;padding:.85rem;background:rgba(124,58,237,.07);border:1px solid rgba(124,58,237,.2);border-radius:.85rem;text-align:center}}
  .auto-status span{{font-size:.85rem;color:var(--text-muted)}}
  .spinner{{width:22px;height:22px;border:2.5px solid rgba(124,58,237,.18);border-top-color:var(--accent2);border-radius:50%;animation:spin .85s linear infinite}}
  @keyframes spin{{to{{transform:rotate(360deg)}}}}
  .btn-link{{background:none;border:none;color:var(--text-dim);font-size:.73rem;cursor:pointer;text-decoration:underline;padding:0;margin-top:.2rem;transition:color .2s}}
  .btn-link:hover{{color:var(--accent2)}}
  /* Forms */
  label{{font-size:.72rem;font-weight:700;color:var(--text-muted);letter-spacing:.06em;text-transform:uppercase}}
  input,textarea{{width:100%;background:var(--surface2);border:1px solid var(--border);border-radius:.65rem;padding:.7rem .9rem;color:var(--text);font-family:'JetBrains Mono',monospace;font-size:.8rem;outline:none;resize:vertical;transition:border-color .2s,box-shadow .2s}}
  input:focus,textarea:focus{{border-color:var(--accent);box-shadow:0 0 0 3px rgba(124,58,237,.14)}}
  input::placeholder,textarea::placeholder{{color:var(--text-dim)}}
  .field{{display:flex;flex-direction:column;gap:.45rem}}
  /* Buttons */
  .btn{{display:flex;align-items:center;justify-content:center;gap:.5rem;width:100%;padding:.75rem 1.25rem;border:none;border-radius:.75rem;font-weight:600;font-size:.88rem;cursor:pointer;transition:all .25s;letter-spacing:.01em}}
  .btn-primary{{background:linear-gradient(135deg,var(--accent),var(--accent2));color:#fff;box-shadow:0 4px 22px var(--accent-glow)}}
  .btn-primary:hover{{transform:translateY(-2px);box-shadow:0 8px 32px var(--accent-glow)}}
  .btn-primary:active{{transform:translateY(0)}}
  .btn-cashu{{background:linear-gradient(135deg,#d97706,var(--cashu));color:#fff;box-shadow:0 4px 22px var(--cashu-glow)}}
  .btn-cashu:hover{{transform:translateY(-2px);box-shadow:0 8px 32px var(--cashu-glow)}}
  .btn-cashu:active{{transform:translateY(0)}}
  /* Cashu section */
  .cashu-header{{display:flex;align-items:center;gap:.85rem}}
  .cashu-icon{{font-size:2rem;line-height:1}}
  .cashu-title{{font-weight:600;font-size:.95rem;color:var(--text);margin-bottom:.15rem}}
  .cashu-subtitle{{font-size:.78rem;color:var(--text-muted)}}
  .payment-req-box{{background:rgba(245,158,11,.06);border:1px solid rgba(245,158,11,.2);border-radius:.65rem;padding:.65rem .85rem;display:flex;flex-direction:column;gap:.3rem;cursor:pointer}}
  .payment-req-box:hover{{border-color:rgba(245,158,11,.45)}}
  .payment-req-label{{font-size:.68rem;font-weight:700;letter-spacing:.08em;color:#d97706;text-transform:uppercase}}
  .payment-req-code{{font-family:'JetBrains Mono',monospace;font-size:.7rem;color:var(--text-muted);word-break:break-all;line-height:1.5}}
  .mints-box{{background:rgba(139,92,246,.06);border:1px solid rgba(139,92,246,.2);border-radius:.65rem;padding:.65rem .85rem;display:flex;flex-direction:column;gap:.35rem}}
  .mints-label{{font-size:.68rem;font-weight:700;letter-spacing:.08em;color:var(--accent2);text-transform:uppercase}}
  .mints-list{{margin:0;padding-left:1.1rem;display:flex;flex-direction:column;gap:.2rem}}
  .mints-list li{{font-family:'JetBrains Mono',monospace;font-size:.72rem;color:var(--text-muted);word-break:break-all}}
  /* Misc */
  .error-msg{{background:rgba(239,68,68,.09);border:1px solid rgba(239,68,68,.22);border-radius:.55rem;padding:.55rem .8rem;font-size:.8rem;color:#f87171}}
  .error-msg.hidden{{display:none}}
  .divider{{border:none;border-top:1px solid var(--border);margin:.2rem 0}}
  .footer{{text-align:center;font-size:.7rem;color:var(--text-dim);padding-top:.25rem}}
  .footer a{{color:var(--text-dim);text-decoration:none;transition:color .2s}}
  .footer a:hover{{color:var(--accent2)}}
</style>
</head>
<body>
<div class="container">
  <!-- Header -->
  <div class="header">
    <div class="badge">&#9889; 402 Payment Required</div>
    <h1>Lightning Payment</h1>
    <p>Access to this resource requires a payment</p>
    <div class="amount">
      <span class="amount-num">{amount_sats}</span>
      <span class="amount-unit">sats ({amount_msat} msats)</span>
    </div>
  </div>
  <!-- Tabs -->
  <div class="tabs">
    <button class="tab-btn active" id="tab-btn-lightning" onclick="switchTab('lightning')">&#9889; LIGHTNING</button>
    <!-- tab-btn-ecash injected below if cashu is enabled -->
    {ecash_tab_btn}
  </div>
  <!-- Lightning Tab -->
  <div id="tab-lightning" class="tab-panel">
    <div class="card">
      <div class="qr-wrap">{qr_svg}</div>
      <div class="invoice-box" onclick="copyInvoice()" title="Click to copy full invoice">
        <span class="invoice-text">{invoice_short}</span>
        <span class="copy-icon">&#10697;</span>
      </div>
      <div class="copy-toast" id="copy-toast">Copied to clipboard!</div>
      <hr class="divider">
      {auto_detect_section}
      <div id="preimage-section" class="{preimage_hidden_class}">
        <div class="field">
          <label for="preimage-input">After paying, enter the preimage</label>
          <input id="preimage-input" type="text" placeholder="Enter preimage (hex)" autocomplete="off" spellcheck="false">
        </div>
        <button class="btn btn-primary" onclick="submitPreimage()">Submit Payment</button>
        <div id="preimage-error" class="error-msg hidden"></div>
      </div>
    </div>
  </div>
  {cashu_tab_html}
  <div class="footer">
    Secured by <a href="https://github.com/ngx-l402/ngx-l402" target="_blank" rel="noopener">ngx_l402</a> &#183; L402 Protocol
  </div>
</div>
<script>
  const INVOICE = {invoice_json};
  const MACAROON = {macaroon_json};
  function switchTab(name) {{
    ['lightning','ecash'].forEach(t => {{
      const panel = document.getElementById('tab-' + t);
      const btn   = document.getElementById('tab-btn-' + t);
      if (!panel || !btn) return;
      if (t === name) {{ panel.classList.remove('hidden'); btn.classList.add('active'); }}
      else {{ panel.classList.add('hidden'); btn.classList.remove('active'); }}
    }});
  }}
  function copyInvoice() {{
    navigator.clipboard.writeText(INVOICE).then(() => {{
      const t = document.getElementById('copy-toast');
      t.style.opacity = '1'; setTimeout(() => t.style.opacity = '0', 2000);
    }}).catch(() => {{
      const box = document.createElement('textarea');
      box.value = INVOICE; document.body.appendChild(box); box.select();
      document.execCommand('copy'); document.body.removeChild(box);
      const t = document.getElementById('copy-toast');
      t.style.opacity = '1'; setTimeout(() => t.style.opacity = '0', 2000);
    }});
  }}
  // Read from the DOM rather than interpolating into JS: the value is already
  // escaped into the page, so there is no second escaping context to get wrong.
  function copyPaymentReq() {{
    const el = document.getElementById('cashu-preq-full');
    if (!el) return;
    const text = el.textContent;
    const toast = () => {{
      const t = document.getElementById('preq-toast');
      t.style.opacity = '1'; setTimeout(() => t.style.opacity = '0', 2000);
    }};
    navigator.clipboard.writeText(text).then(toast).catch(() => {{
      const box = document.createElement('textarea');
      box.value = text; document.body.appendChild(box); box.select();
      document.execCommand('copy'); document.body.removeChild(box);
      toast();
    }});
  }}
  function showContent(r) {{
    r.text().then(html => {{ document.open(); document.write(html); document.close(); }});
  }}
  function submitPreimage() {{
    const hex = document.getElementById('preimage-input').value.trim();
    const errEl = document.getElementById('preimage-error');
    if (!/^[0-9a-fA-F]{{64}}$/.test(hex)) {{
      errEl.textContent = 'Invalid preimage \u2014 must be 64 hex characters.';
      errEl.classList.remove('hidden'); return;
    }}
    errEl.classList.add('hidden');
    const btn = event.currentTarget; btn.textContent = 'Verifying\u2026'; btn.disabled = true;
    fetch(window.location.href, {{
      method: 'GET',
      headers: {{'Authorization': 'L402 ' + MACAROON + ':' + hex}},
      redirect: 'follow', credentials: 'same-origin'
    }}).then(r => {{
      if (r.ok || r.status === 200) {{ showContent(r); }}
      else {{
        errEl.textContent = 'Payment verification failed (status ' + r.status + '). Check your preimage.';
        errEl.classList.remove('hidden'); btn.textContent = 'Submit Payment'; btn.disabled = false;
      }}
    }}).catch(e => {{
      errEl.textContent = 'Network error: ' + e.message;
      errEl.classList.remove('hidden'); btn.textContent = 'Submit Payment'; btn.disabled = false;
    }});
  }}
  function submitCashu() {{
    const token = document.getElementById('cashu-token').value.trim();
    const errEl = document.getElementById('cashu-error');
    if (!token.startsWith('cashu')) {{
      errEl.textContent = 'Invalid Cashu token \u2014 must start with "cashu".';
      errEl.classList.remove('hidden'); return;
    }}
    errEl.classList.add('hidden');
    const btn = event.currentTarget; btn.textContent = 'Verifying\u2026'; btn.disabled = true;
    fetch(window.location.href, {{
      method: 'GET',
      headers: {{'Authorization': 'Cashu ' + token}},
      redirect: 'follow', credentials: 'same-origin'
    }}).then(r => {{
      if (r.ok || r.status === 200) {{ showContent(r); }}
      else {{
        errEl.textContent = 'Token verification failed (status ' + r.status + ').';
        errEl.classList.remove('hidden'); btn.textContent = 'Submit Token'; btn.disabled = false;
      }}
    }}).catch(e => {{
      errEl.textContent = 'Network error: ' + e.message;
      errEl.classList.remove('hidden'); btn.textContent = 'Submit Token'; btn.disabled = false;
    }});
  }}
  {auto_detect_js}
</script>
</body>
</html>"#,
        amount_sats = amount_sats,
        amount_msat = amount_msat,
        ecash_tab_btn = ecash_tab_btn,
        qr_svg = qr_svg,
        invoice_short = invoice_short,
        auto_detect_section = auto_detect_section,
        preimage_hidden_class = preimage_hidden_class,
        cashu_tab_html = cashu_tab_html,
        invoice_json = script_json_literal(invoice),
        macaroon_json = script_json_literal(macaroon_b64),
        auto_detect_js = auto_detect_js,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const INVOICE: &str = "lnbc100n1pjabcdefgqqqqqqqqqqqqqqqqqqqqqqqqqqqq";
    const MACAROON: &str = "AgELbmd4X2w0MDIuaW8CQgAA";

    fn render(auto_detect: bool, cashu_enabled: bool, cashu_req: Option<&str>) -> String {
        render_with_mints(auto_detect, cashu_enabled, cashu_req, &[])
    }

    fn render_with_mints(
        auto_detect: bool,
        cashu_enabled: bool,
        cashu_req: Option<&str>,
        mints: &[String],
    ) -> String {
        render_payment_page(
            INVOICE,
            10_000,
            MACAROON,
            auto_detect,
            cashu_enabled,
            cashu_req,
            mints,
        )
    }

    #[test]
    fn auto_detect_on_polls_and_hides_manual_entry() {
        let html = render(true, false, None);
        assert!(
            html.contains("startPolling()"),
            "polling JS must be injected"
        );
        assert!(
            html.contains("id=\"auto-status\""),
            "the waiting spinner must be present"
        );
        // The manual field still exists — the fallback below reveals it — but it
        // starts hidden so the two flows are not offered at once.
        assert!(
            html.contains("id=\"preimage-section\" class=\"hidden\""),
            "manual preimage entry must start hidden while polling"
        );
    }

    #[test]
    fn auto_detect_off_shows_manual_entry_and_no_polling() {
        let html = render(false, false, None);
        assert!(
            !html.contains("startPolling"),
            "no polling JS without auto-detect"
        );
        assert!(
            !html.contains("id=\"auto-status\""),
            "no spinner without auto-detect"
        );
        assert!(
            html.contains("id=\"preimage-input\""),
            "manual preimage entry must be offered"
        );
        assert!(
            !html.contains("id=\"preimage-section\" class=\"hidden\""),
            "manual entry must be visible, not hidden"
        );
    }

    /// After MAX_POLL attempts the spinner is dismissed and manual entry is
    /// revealed, so a stalled detector never leaves the payer stuck.
    #[test]
    fn poll_fallback_reveals_manual_entry() {
        let html = render(true, false, None);
        assert!(html.contains("MAX_POLL"), "a poll ceiling must exist");
        assert!(
            html.contains("pollAttempts++ > MAX_POLL"),
            "the ceiling must be enforced"
        );
        assert!(
            html.contains("document.getElementById('preimage-section').classList.remove('hidden')"),
            "hitting the ceiling must reveal manual preimage entry"
        );
    }

    #[test]
    fn cashu_tab_appears_only_when_enabled() {
        let with = render(false, true, None);
        assert!(
            with.contains("id=\"tab-ecash\""),
            "Cashu panel when enabled"
        );
        assert!(
            with.contains("id=\"cashu-token\""),
            "Cashu input when enabled"
        );

        let without = render(false, false, None);
        assert!(
            !without.contains("id=\"tab-ecash\""),
            "no Cashu panel when off"
        );
        assert!(
            !without.contains("id=\"cashu-token\""),
            "no Cashu input when off"
        );
    }

    #[test]
    fn cashu_payment_request_is_shown_only_when_supplied() {
        let with = render(false, true, Some("creqAxyz123"));
        assert!(
            with.contains("creqAxyz123"),
            "the request must be previewed"
        );
        assert!(
            with.contains("Payment Request"),
            "the preview box must render"
        );

        // The `payment-req-*` classes are always in the stylesheet, so assert on
        // the rendered box instead of the class name.
        let without = render(false, true, None);
        assert!(
            !without.contains("Payment Request"),
            "no preview box without a request"
        );
        assert!(!without.contains("creqA"));
    }

    /// A truncated request cannot be pasted into a wallet, so the full string
    /// has to be in the DOM for the copy handler to read.
    #[test]
    fn payment_request_is_present_in_full_not_only_previewed() {
        let long = format!("creqA{}", "x".repeat(400));
        let html = render(false, true, Some(&long));
        assert!(
            html.contains(&long),
            "the full request must be in the page, not just a preview"
        );
        assert!(
            html.contains("copyPaymentReq()"),
            "and it must be copyable, like the invoice"
        );
    }

    /// Without this the mints reach NUT-24 wallets through the X-Cashu header
    /// and nobody else: a person pastes a token from an unlisted mint and finds
    /// out only from the 400.
    #[test]
    fn whitelisted_mints_are_listed_for_the_reader() {
        let mints = vec![
            "https://mint.example.com".to_string(),
            "https://other.example.org/".to_string(),
        ];
        let html = render_with_mints(false, true, None, &mints);
        assert!(html.contains("mint.example.com"), "first mint must show");
        assert!(html.contains("other.example.org"), "second mint must show");
        assert!(
            !html.contains("https://mint.example.com"),
            "hosts only — a full URL wraps badly on a phone"
        );
        assert!(html.contains("accepted from these mints only"));
    }

    /// An empty whitelist means every mint is accepted, so naming none is the
    /// honest rendering — claiming a restriction that isn't there would be worse
    /// than saying nothing.
    #[test]
    fn no_mint_box_when_the_whitelist_is_empty() {
        let html = render(false, true, None);
        // `.mints-box` is always in the stylesheet, so assert on the rendered
        // element and its copy, as the payment-request test above does.
        assert!(!html.contains("<div class=\"mints-box\""));
        assert!(!html.contains("accepted from these mints only"));
    }

    #[test]
    fn mint_urls_are_escaped() {
        let mints = vec!["https://evil.example/<img src=x onerror=alert(1)>".to_string()];
        let html = render_with_mints(false, true, None, &mints);
        assert!(!html.contains("<img src=x"), "mint URLs must be escaped");
        assert!(html.contains("&lt;img"));
    }

    /// The invoice and macaroon are interpolated into an inline <script>. A
    /// value containing `</script>` must not be able to close the element —
    /// the tokenizer ends it at the first literal `</script`, whatever the JS
    /// string context.
    #[test]
    fn script_context_cannot_be_broken_out_of() {
        let hostile = "</script><img src=x onerror=alert(1)>";
        let hostile_mints = vec![hostile.to_string()];
        let html = render_payment_page(
            hostile,
            10_000,
            hostile,
            true,
            true,
            Some(hostile),
            &hostile_mints,
        );
        assert!(
            !html.to_ascii_lowercase().contains("</script><img"),
            "a script-closing sequence survived into the page"
        );
        assert!(
            !html.contains("onerror=alert(1)>"),
            "unescaped attacker markup reached the output"
        );
    }

    #[test]
    fn amount_is_rendered_in_sats() {
        // 10_000 msat = 10 sat.
        let html = render(false, false, None);
        assert!(html.contains(">10<") || html.contains("10 sat") || html.contains("10</"));
    }
}
