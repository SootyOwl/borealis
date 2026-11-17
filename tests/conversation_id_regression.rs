use borealis::core::event::ConversationId;

#[test]
fn system_conversation_names_preserve_colons() {
    for name in [
        "reminder:daily",
        ":",
        ":start",
        "end:",
        "a::b",
        "réveil:朝",
        "",
    ] {
        let id = ConversationId::System {
            event_name: name.to_string(),
        };
        assert_eq!(ConversationId::parse(&id.to_string()), Ok(id));
    }
}
