fn meta() {
    #{
        id: "apple_ipad",
        name: "Apple iPad",
        category: "tablet",
        category_title: "Tablets",
        category_icon: "📟",
        capabilities: ["wifi", "airplay"]
    }
}

fn identify(obs) {
    let name = obs.name.to_lower();
    let host = obs.hostname.to_lower();
    name.contains("ipad") || name.contains("tablet") || name.contains("tab")
        || host.contains("ipad") || host.contains("tablet")
}
