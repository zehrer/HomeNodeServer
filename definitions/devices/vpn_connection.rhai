fn meta() {
    #{
        id: "vpn_connection",
        name: "WireGuard / IPSec VPN Virtual Device",
        category: "vpn",
        category_title: "VPN & Virtual Devices",
        category_icon: "🛡️",
        capabilities: ["vpn", "wireguard", "ipsec", "virtual", "remote-access"]
    }
}

fn identify(obs) {
    let name = obs.name.to_lower();
    let host = obs.hostname.to_lower();
    let iface = obs.interface.to_lower();

    name.contains("vpn")
        || host.contains("vpn")
        || name.contains("wireguard")
        || host.contains("wireguard")
        || iface == "vpn"
        || iface == "virtual"
        || host.contains("iphonestephan")
        || name.contains("iphonestephan")
}
