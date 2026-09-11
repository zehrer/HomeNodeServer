fn meta() {
    #{
        id: "snom_phone",
        name: "Snom VoIP Phone",
        category: "voip-phone",
        category_title: "VoIP Phones",
        category_icon: "☎️",
        capabilities: ["sip", "voip", "ethernet"]
    }
}

fn identify(obs) {
    let name = obs.name.to_lower();
    name.contains("snom") || name.contains("voip") || name.contains("sip")
}
