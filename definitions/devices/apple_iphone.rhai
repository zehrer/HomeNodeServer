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
    name.contains("iphone")
}
