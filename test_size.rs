use deepseek_acp_adapter::deepseek::{ChatMessage, ChatRequest};

fn main() {
    // Create a test message like the ones in the conversation
    let msg = ChatMessage::user("This is a test message with some content that's reasonably long. Let's add more content to simulate realistic messages. ".repeat(100));
    
    // Simulate the kind of request that would be created
    let messages = vec![msg];
    let request = ChatRequest::new(messages);
    
    // Try to estimate what the serialized size would be
    println!("Messages count: {}", request.len());
}
