use serde::{Deserialize, Serialize};

#[derive(Clone,Serialize, Deserialize)]
pub struct User {
    pub username: String,
    pub password: String,
}
