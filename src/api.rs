use salvo::prelude::*;

#[handler]
async fn hello(res: &mut Response) {
    res.render(Text::Plain("Zonemail API is running!"));
}

pub fn create_router() -> Router {
    Router::new().get(hello)
}
