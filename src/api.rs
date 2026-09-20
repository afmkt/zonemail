use salvo::prelude::*;

#[endpoint(
    summary = "Create a new tenant",
    // request_body = NewTenant,
    // responses(
    //     (status_code = 200, description = "Tenant created successfully", body = ApiResponse<()>),
    //     (status_code = 400, description = "Bad request", body = ApiProblem)
    // )
)]

async fn hello(res: &mut Response) {
    res.render(Text::Plain("Zonemail API is running!"));
}

pub fn create_router() -> Router {
    Router::new().get(hello)
}
