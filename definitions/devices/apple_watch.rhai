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
    name.contains("watch") || name.contains("band") || name.contains("wearable")
}
