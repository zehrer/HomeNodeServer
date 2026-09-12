fn meta() {
    #{
        id: "apple_iphone",
        name: "Apple iPhone",
        category: "phone",
        category_title: "Smartphones",
        category_icon: "📱",
        capabilities: ["wifi", "airplay", "mobile-scout"]
    }
}

fn identify(obs) {
    let name = obs.name.to_lower();
    let host = obs.hostname.to_lower();
    let iface = obs.interface.to_lower();

    if iface == "vpn"
        || name.contains("vpn")
        || host.contains("vpn")
        || name.contains("wireguard")
        || host.contains("wireguard")
        || name.contains("iphonestephan")
        || host.contains("iphonestephan")
    {
        return false;
    }

    name.contains("iphone") || host.contains("iphone")
}
