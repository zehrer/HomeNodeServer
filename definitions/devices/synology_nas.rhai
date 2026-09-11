fn meta() {
    #{
        id: "synology_nas",
        name: "Synology NAS",
        category: "nas",
        category_title: "Network Storage & NAS",
        category_icon: "🗄️",
        capabilities: ["storage", "backup", "smb", "nfs", "http"]
    }
}

fn identify(obs) {
    let name = obs.name.to_lower();
    let host = obs.hostname.to_lower();
    let vendor = obs.vendor.to_lower();

    name.contains("synology")
        || host.contains("synology")
        || host.contains("diskstation")
        || host.contains("rackstation")
        || vendor.contains("synology")
}
