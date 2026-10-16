use snak_chat::Conversation;

pub fn matches(conversation: &Conversation, query: &str) -> bool {
    if query.is_empty() {
        return true;
    }
    let query = query.to_lowercase();
    conversation.title.to_lowercase().contains(&query)
        || conversation
            .group()
            .is_some_and(|g| g.name.to_lowercase().contains(&query))
}
