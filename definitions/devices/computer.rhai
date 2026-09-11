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
    name.contains("macbook") || name.contains("imac") || name.contains("macmini") || name.contains("desktop") || name.contains("laptop") || name.contains("pc")
}
