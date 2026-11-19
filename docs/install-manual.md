# Manual Installation

> **Note**: This module requires **NGINX 1.30.7+** (stable) or **1.31.9+** (mainline). Older releases lack the fixes for the HTTP/2 bugs CVE-2026-123007 and CVE-2026-123633.

## Steps

### 1. Download the Module

Download `libngx_l402_lib.so` from the [latest release](https://github.com/ngx-l402/ngx-l402/releases/latest) and copy it to your Nginx modules directory:

```bash
sudo cp libngx_l402_lib.so /etc/nginx/modules/
```

### 2. Load the Module in nginx.conf

```nginx
load_module /etc/nginx/modules/libngx_l402_lib.so;
```

### 3. Enable L402 for Specific Locations

```nginx
location /protected {
    root   /usr/share/nginx/html;
    index  index.html index.htm;
    
    # L402 module directives:
    l402 on;
    l402_amount_msat_default    10000;
    # Note: Dynamic pricing is handled via Redis using the request path as key
    # Example: SET /protected 15000 (sets price to 15000 msats for /protected endpoint)
    l402_macaroon_timeout 3600;  # Macaroon validity in seconds, set to 0 to disable timeout
    # Optional: per-location LNURL address for multi-tenant setups
    # l402_lnurl_addr "tenant@your-lnurl-server.com";
}
```

### 4. Set Environment Variables

Set the following in `nginx.service` (typically `/lib/systemd/system/nginx.service`).

See [Environment Variables](./config-env-vars.md) for the complete reference.

### 5. Set Up SQLite Database Directory (if using Cashu)

```bash
# One-time setup — persists across restarts
sudo mkdir -p /var/lib/nginx
# Root owns it, nginx writes through the group, and the sticky bit keeps nginx
# away from the root-owned wallet phrase — see Cashu eCash
sudo chown root:nginx /var/lib/nginx
sudo chmod 1770 /var/lib/nginx
```

> The `cdk-sqlite` crate automatically creates the database file and tables on first run. Database location: `/var/lib/nginx/cashu_tokens.db`

### 6. Restart Nginx

```bash
sudo systemctl restart nginx
```
