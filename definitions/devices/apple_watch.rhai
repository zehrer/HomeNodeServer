fn meta() {
    #{
        id: "apple_watch",
        name: "Apple Watch / Wearable",
        category: "wearable",
        category_title: "Wearables",
        category_icon: "⌚",
        capabilities: ["ble", "wifi"]
    }
}

fn identify(obs) {
    let name = obs.name.to_lower();
    let host = obs.hostname.to_lower();
    name.contains("watch") || host.contains("watch")
        || name.contains("band") || host.contains("band")
        || name.contains("wearable") || host.contains("wearable")
}
