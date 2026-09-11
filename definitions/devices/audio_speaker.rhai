fn meta() {
    #{
        id: "audio_speaker",
        name: "Smart Speaker / Audio",
        category: "audio",
        category_title: "Audio & Speakers",
        category_icon: "🔊",
        capabilities: ["audio", "airplay"]
    }
}

fn identify(obs) {
    let name = obs.name.to_lower();
    name.contains("homepod") || name.contains("sonos") || name.contains("speaker") || name.contains("echo")
}
