fn meta() {
    #{
        id: "computer",
        name: "Computer / Laptop",
        category: "computer",
        category_title: "Computers & Laptops",
        category_icon: "💻",
        capabilities: ["lan", "ssh"]
    }
}

fn identify(obs) {
    let name = obs.name.to_lower();
    let host = obs.hostname.to_lower();

    if name.contains("macbook") || name.contains("imac") || name.contains("macmini") {
        return true;
    }
    if name.contains("desktop") || name.contains("laptop") || name.contains("pc") {
        return true;
    }
    if name.contains("mbp") || name.contains("mba") {
        return true;
    }
    if host.contains("macbook") || host.contains("imac") || host.contains("macmini") {
        return true;
    }
    if host.contains("mbp") || host.contains("mba") {
        return true;
    }
    false
}
