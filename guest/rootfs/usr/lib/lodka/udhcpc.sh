#!/bin/sh
# udhcpc's hook: eth0's address, the default route and DNS from the vmnet network's lease.
case $1 in
  deconfig) ip -4 addr flush dev "$interface" ;;
  bound | renew)
    [ "$1" = bound ] && ip -4 addr flush dev "$interface"
    ip addr replace "$ip/${mask:-24}" dev "$interface"
    ip route replace default via "${router%% *}" dev "$interface"
    : >/etc/resolv.conf
    for x in $dns; do echo "nameserver $x" >>/etc/resolv.conf; done
    [ -n "$domain" ] && echo "search $domain" >>/etc/resolv.conf
    ;;
esac
exit 0
