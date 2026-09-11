fn meta() {
    #{
        id: "meshtastic",
        name: "Meshtastic Node / Gateway",
        category: "radio",
        category_title: "LoRa & Mesh Radios",
        category_icon: "📻",
        capabilities: ["lora", "mesh", "ble", "wifi", "http"]
    }
}

fn identify(obs) {
    let name = obs.name.to_lower();
    let host = obs.hostname.to_lower();

    name.contains("meshtastic") || host.contains("meshtastic")
}
