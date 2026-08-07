# NetFyr Server — produktionsinstallation
#
# Kör som root på servern. Skapar tjänstanvändaren netfyr, lägger
# binären i /usr/local/bin, konfigurationen i /etc/netfyr och datan i
# /var/lib/netfyr.
#
#   ./deploy/install.sh
#
set -euo pipefail

cd "$(dirname "$0")/.."

if [ ! -f target/release/netfyr-server ]; then
    echo "release-binären saknas — kör först: cargo build --release" >&2
    exit 1
fi

# Tjänstanvändare utan inloggningsskal.
if ! id netfyr >/dev/null 2>&1; then
    useradd --system --no-create-home --shell /usr/sbin/nologin netfyr
    echo "användare skapad: netfyr"
fi

install -d -m 0750 -o netfyr -g netfyr /var/lib/netfyr
install -d -m 0755 /etc/netfyr

install -m 0755 target/release/netfyr-server /usr/local/bin/netfyr-server

# Frontend-filerna.
install -d -m 0755 /usr/local/share/netfyr/web
install -m 0644 web/* /usr/local/share/netfyr/web/

if [ ! -f /etc/netfyr/config.toml ]; then
    install -m 0640 -o root -g netfyr config.example.toml /etc/netfyr/config.toml
    echo "konfiguration lagd: /etc/netfyr/config.toml — redigera den!"
else
    echo "/etc/netfyr/config.toml finns redan, lämnas orörd"
fi

# ICMP-datagramsocketar för tjänstanvändaren (se README etapp 2).
GID=$(id -g netfyr)
sysctl -w "net.ipv4.ping_group_range=$GID $GID"
echo "net.ipv4.ping_group_range=$GID $GID" > /etc/sysctl.d/90-netfyr.conf

install -m 0644 deploy/netfyr.service /etc/systemd/system/netfyr.service
systemctl daemon-reload
systemctl enable netfyr
systemctl restart netfyr

echo
echo "Done. The one-time admin password is in the service journal:"
echo "  journalctl -u netfyr -n 30 | grep -A4 'FIRST RUN'"
echo "Change it at first login."
