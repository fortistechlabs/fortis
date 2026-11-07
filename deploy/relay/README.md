# TCP relay for fortis-lsp (small VPS in front of a home LSP)

Use this when the home connection cannot take inbound connections (CGNAT /
IPv6-only). The VPS has a public IPv4 address and forwards TCP 9735 (and 9835 for the XBT LSP) to the LSP
at home over a WireGuard tunnel that **the home machine opens outbound**.
The VPS holds no seed, no funds and no Bitcoin Core: it only forwards bytes
(BOLT 8 is encrypted end to end), so losing or reclaiming it costs nothing but
uptime. The LSP keeps running at home.

```
phone ──TCP 9735──▶ VPS (public IPv4, nftables DNAT) ══WireGuard══▶ home 10.8.0.2:9735 (fortis-lsp)
```

Addresses used below: tunnel 10.8.0.1 (VPS) and 10.8.0.2 (home), UDP 51820.

## 1. VM (Google Cloud e2-micro, Always Free)

- Machine type `e2-micro`, region `us-west1`, `us-central1` or `us-east1`
  (the free tier only covers those), Debian 12, 30 GB standard disk.
- Reserve a **static external IPv4** and attach it, so it survives restarts.
  External IPv4 addresses can be billed separately from the free VM: check
  Billing and set a budget alert.
- VPC firewall rules (ingress, source 0.0.0.0/0): `tcp:9735`, `tcp:9835` (the XBT LSP) and `udp:51820`.
- Egress above the free monthly allowance is billed; Lightning traffic is small.
- The region adds latency for European users; the relay itself adds nothing else.

## 2. VPS side

```sh
sudo apt update && sudo apt install -y wireguard nftables
sudo sh -c 'umask 077; wg genkey | tee /etc/wireguard/vps.key | wg pubkey > /etc/wireguard/vps.pub'
sudo cat /etc/wireguard/vps.pub        # → needed on the home side
```
Install `vps-wg0.conf` as `/etc/wireguard/wg0.conf` and `lnrelay.nft` as
`/etc/wireguard/lnrelay.nft`, fill in the keys, then:
```sh
sudo systemctl enable --now wg-quick@wg0
```

## 3. Home side (this machine)

```sh
sudo pacman -S --needed wireguard-tools
sudo sh -c 'umask 077; wg genkey | tee /etc/wireguard/home.key | wg pubkey > /etc/wireguard/home.pub'
sudo cat /etc/wireguard/home.pub       # → needed on the VPS side
```
Install `home-wg0.conf` as `/etc/wireguard/wg0.conf` (mode 0600), fill in the
keys and the VPS IP, then `sudo systemctl enable --now wg-quick@wg0`.
It routes only 10.8.0.1/32 through the tunnel, never your other traffic.

## 4. Check

```sh
ping -c2 10.8.0.1                         # home → VPS
sudo wg show                              # "latest handshake" within a minute
nc -vz <VPS_IP> 9735                      # from outside (phone on mobile data)
nc -vz <VPS_IP> 9835                      # the XBT LSP, same check
```

## 5. DNS

Point `lsp.fortistechlabs.com` (A record, DNS only) at the VPS's static IPv4.
Delete any A record pointing at the ISP's shared address.

## Notes

- The LSP sees every client as 10.8.0.1 (masquerade). Peers are identified by
  node id, so nothing depends on source IPs.
- If the tunnel is down the relay accepts connections and they fail; the edge
  `ln-info.json` kill switch (`"enabled": false`) is the place to say so.
- The LSP's own outbound channel peers still see your home IP.
