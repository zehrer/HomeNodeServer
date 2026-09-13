fn meta() {
    #{
        id: "network_switch",
        name: "Network Switch",
        category: "switch",
        category_title: "Network Switches",
        category_icon: "🔀",
        capabilities: ["ethernet", "switch"]
    }
}

fn identify(obs) {
    let name = obs.name.to_lower();
    let host = obs.hostname.to_lower();
    let mac = obs.mac.to_lower();

    // Do not match SwitchBot or Nintendo Switch
    if name.contains("switchbot") || host.contains("switchbot") || name.contains("nintendo") || host.contains("nintendo") {
        return false;
    }

    mac.starts_with("fa:ce:")
        || name == "switch"
        || host == "switch"
        || name.starts_with("switch-")
        || name.starts_with("switch_")
        || host.starts_with("switch-")
        || host.starts_with("switch_")
        || host.ends_with("-switch")
        || name.contains("gigabit switch")
        || name.contains("network switch")
        || name.contains("managed switch")
}
