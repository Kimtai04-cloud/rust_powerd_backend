
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post, put, delete},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use sqlx::{sqlite::SqlitePool, Row, Executor, Transaction};
use std::{net::SocketAddr, sync::Arc};
use tracing::{info, error};
use tracing_subscriber::EnvFilter;
use dotenvy::dotenv;
use uuid::Uuid;
use thiserror::Error;
use tower_http::cors::{Any, CorsLayer};
use chrono::Utc;
use serde_json::json;

#[derive(Clone)]
struct AppState {
    pool: SqlitePool,
}

macro_rules! json {
    ($($tt:tt)*) => { serde_json::json!($($tt)*) };
}

// ---------- Models ----------

#[derive(Debug, Serialize, Deserialize)]
struct Product {
    id: i64,
    name: String,
    description: Option<String>,
    price_cents: i64,
    stock: i32,
    created_at: String,
}

#[derive(Debug, Deserialize)]
struct CreateProduct {
    name: String,
    description: Option<String>,
    price_cents: i64,
    stock: i32,
}

#[derive(Debug, Deserialize)]
struct UpdateProduct {
    name: Option<String>,
    description: Option<String>,
    price_cents: Option<i64>,
    stock: Option<i32>,
}

#[derive(Debug, Deserialize)]
struct OrderItemRequest {
    product_id: i64,
    quantity: i32,
}

#[derive(Debug, Deserialize)]
struct CreateOrder {
    items: Vec<OrderItemRequest>,
}

#[derive(Debug, Serialize)]
struct OrderResponse {
    id: String,
    total_cents: i64,
}

// ---------- Error handling ----------

#[derive(Error, Debug)]
enum AppError {
    #[error("Not found")] NotFound,
    #[error("Bad request: {0}")] BadRequest(String),
    #[error("Database error")] DbError(#[from] sqlx::Error),
    #[error("Internal error")] InternalError,
}

impl IntoResponse for AppError {
    fn into_response(self) -> axum::response::Response {
        let (status, body) = match &self {
            AppError::NotFound => (StatusCode::NOT_FOUND, Json(json!({"error": "Not Found"}))),
            AppError::BadRequest(msg) => (StatusCode::BAD_REQUEST, Json(json!({"error": msg}))),
            AppError::DbError(e) => {
                error!("db error: {}", e);
                (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": "Database error"})))
            }
            AppError::InternalError => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": "Internal error"}))),
        };
        (status, body).into_response()
    }
}

// A small helper macro (since we didn't import serde_json::json directly in scope)

// ---------- Handlers ----------

async fn list_products(State(state): State<Arc<AppState>>) -> Result<Json<Vec<Product>>, AppError> {
    let rows = sqlx::query("SELECT id, name, description, price_cents, stock, created_at FROM products ORDER BY id DESC")
        .fetch_all(&state.pool)
        .await?;

    let products: Vec<Product> = rows
        .into_iter()
        .map(|r| Product {
            id: r.get::<i64, _>("id"),
            name: r.get::<String, _>("name"),
            description: r.get::<Option<String>, _>("description"),
            price_cents: r.get::<i64, _>("price_cents"),
            stock: r.get::<i32, _>("stock"),
            created_at: r.get::<String, _>("created_at"),
        })
        .collect();

    Ok(Json(products))
}

async fn get_product(Path(id): Path<i64>, State(state): State<Arc<AppState>>) -> Result<Json<Product>, AppError> {
    let row = sqlx::query("SELECT id, name, description, price_cents, stock, created_at FROM products WHERE id = ?")
        .bind(id)
        .fetch_optional(&state.pool)
        .await?;

    match row {
        Some(r) => Ok(Json(Product {
            id: r.get::<i64, _>("id"),
            name: r.get::<String, _>("name"),
            description: r.get::<Option<String>, _>("description"),
            price_cents: r.get::<i64, _>("price_cents"),
            stock: r.get::<i32, _>("stock"),
            created_at: r.get::<String, _>("created_at"),
        })),
        None => Err(AppError::NotFound),
    }
}

async fn create_product(State(state): State<Arc<AppState>>, Json(payload): Json<CreateProduct>) -> Result<(StatusCode, Json<Product>), AppError> {
    // basic validations
    if payload.name.trim().is_empty() {
        return Err(AppError::BadRequest("name must not be empty".into()));
    }
    if payload.price_cents <= 0 {
        return Err(AppError::BadRequest("price_cents must be > 0".into()));
    }
    let now = Utc::now().to_rfc3339();
    let mut tx = state.pool.begin().await?;
    let res = sqlx::query("INSERT INTO products (name, description, price_cents, stock, created_at) VALUES (?, ?, ?, ?, ?)")
        .bind(&payload.name)
        .bind(&payload.description)
        .bind(payload.price_cents)
        .bind(payload.stock)
        .bind(&now)
        .execute(&mut tx)
        .await?;

    // For sqlite we can get the last insert id
    let inserted_id = res.last_insert_rowid();

    tx.commit().await?;

    // fetch created
    let row = sqlx::query("SELECT id, name, description, price_cents, stock, created_at FROM products WHERE id = ?")
        .bind(inserted_id)
        .fetch_one(&state.pool)
        .await?;

    let product = Product {
        id: row.get("id"),
        name: row.get("name"),
        description: row.get("description"),
        price_cents: row.get("price_cents"),
        stock: row.get("stock"),
        created_at: row.get("created_at"),
    };

    Ok((StatusCode::CREATED, Json(product)))
}

async fn update_product(Path(id): Path<i64>, State(state): State<Arc<AppState>>, Json(payload): Json<UpdateProduct>) -> Result<Json<Product>, AppError> {
    // perform an updatable SQL using COALESCE so that omitted fields keep their existing values
    let mut tx = state.pool.begin().await?;
    let _ = sqlx::query(
        "UPDATE products SET name = COALESCE(?, name), description = COALESCE(?, description), price_cents = COALESCE(?, price_cents), stock = COALESCE(?, stock) WHERE id = ?"
    )
    .bind(payload.name.as_deref())
    .bind(payload.description.as_deref())
    .bind(payload.price_cents)
    .bind(payload.stock)
    .bind(id)
    .execute(&mut tx)
    .await?;

    tx.commit().await?;

    // return updated
    let row = sqlx::query("SELECT id, name, description, price_cents, stock, created_at FROM products WHERE id = ?")
        .bind(id)
        .fetch_optional(&state.pool)
        .await?;

    match row {
        Some(r) => Ok(Json(Product {
            id: r.get("id"),
            name: r.get("name"),
            description: r.get("description"),
            price_cents: r.get("price_cents"),
            stock: r.get("stock"),
            created_at: r.get("created_at"),
        })),
        None => Err(AppError::NotFound),
    }
}

async fn delete_product(Path(id): Path<i64>, State(state): State<Arc<AppState>>) -> Result<StatusCode, AppError> {
    let _ = sqlx::query("DELETE FROM products WHERE id = ?")
        .bind(id)
        .execute(&state.pool)
        .await?;

    Ok(StatusCode::NO_CONTENT)
}

async fn create_order(State(state): State<Arc<AppState>>, Json(payload): Json<CreateOrder>) -> Result<(StatusCode, Json<OrderResponse>), AppError> {
    if payload.items.is_empty() {
        return Err(AppError::BadRequest("order must contain at least one item".into()));
    }

    // Start a transaction so stock checks and updates are atomic
    let mut tx: Transaction<'_, sqlx::Sqlite> = state.pool.begin().await?;

    // We'll calculate total and modify stock
    let mut total_cents: i64 = 0;

    // Step 1: Check stock and compute total
    for item in &payload.items {
        let row = sqlx::query("SELECT stock, price_cents FROM products WHERE id = ?")
            .bind(item.product_id)
            .fetch_optional(&mut tx)
            .await?;

        let row = match row {
            Some(r) => r,
            None => return Err(AppError::BadRequest(format!("product {} not found", item.product_id))),
        };

        let stock: i32 = row.get("stock");
        let unit_price: i64 = row.get("price_cents");

        if stock < item.quantity {
            return Err(AppError::BadRequest(format!("not enough stock for product {}", item.product_id)));
        }

        total_cents += (item.quantity as i64) * unit_price;
    }

    // Step 2: insert order
    let order_id = Uuid::new_v4().to_string();
    let now = Utc::now().to_rfc3339();
    sqlx::query("INSERT INTO orders (id, total_cents, created_at) VALUES (?, ?, ?)")
        .bind(&order_id)
        .bind(total_cents)
        .bind(&now)
        .execute(&mut tx)
        .await?;

    // Step 3: insert order items and decrement stock
    for item in &payload.items {
        // get current price to freeze at time of order
        let row = sqlx::query("SELECT price_cents FROM products WHERE id = ?")
            .bind(item.product_id)
            .fetch_one(&mut tx)
            .await?;
        let unit_price: i64 = row.get("price_cents");

        sqlx::query("INSERT INTO order_items (order_id, product_id, quantity, unit_price_cents) VALUES (?, ?, ?, ?)")
            .bind(&order_id)
            .bind(item.product_id)
            .bind(item.quantity)
            .bind(unit_price)
            .execute(&mut tx)
            .await?;

        sqlx::query("UPDATE products SET stock = stock - ? WHERE id = ?")
            .bind(item.quantity)
            .bind(item.product_id)
            .execute(&mut tx)
            .await?;
    }

    tx.commit().await?;

    Ok((StatusCode::CREATED, Json(OrderResponse { id: order_id, total_cents })))
}

async fn get_order(Path(id): Path<String>, State(state): State<Arc<AppState>>) -> Result<Json<serde_json::Value>, AppError> {
    let row = sqlx::query("SELECT id, total_cents, created_at FROM orders WHERE id = ?")
        .bind(&id)
        .fetch_optional(&state.pool)
        .await?;

    if let Some(r) = row {
        let items = sqlx::query("SELECT product_id, quantity, unit_price_cents FROM order_items WHERE order_id = ?")
            .bind(&id)
            .fetch_all(&state.pool)
            .await?;

        let items_json: Vec<serde_json::Value> = items.into_iter().map(|it| {
            serde_json::json!({
                "product_id": it.get::<i64, _>("product_id"),
                "quantity": it.get::<i32, _>("quantity"),
                "unit_price_cents": it.get::<i64, _>("unit_price_cents"),
            })
        }).collect();

        let resp = serde_json::json!({
            "id": r.get::<String, _>("id"),
            "total_cents": r.get::<i64, _>("total_cents"),
            "created_at": r.get::<String, _>("created_at"),
            "items": items_json,
        });

        Ok(Json(resp))
    } else {
        Err(AppError::NotFound)
    }
}

// ---------- DB init ----------
async fn init_db(pool: &SqlitePool) -> Result<(), sqlx::Error> {
    // Note: using IF NOT EXISTS allows running this at startup without external migration tool.
    let mut conn = pool.acquire().await?;

    conn.execute(
        r#"CREATE TABLE IF NOT EXISTS products (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            name TEXT NOT NULL,
            description TEXT,
            price_cents INTEGER NOT NULL,
            stock INTEGER NOT NULL DEFAULT 0,
            created_at TEXT NOT NULL
        );"#,
    ).await?;

    conn.execute(
        r#"CREATE TABLE IF NOT EXISTS orders (
            id TEXT PRIMARY KEY,
            total_cents INTEGER NOT NULL,
            created_at TEXT NOT NULL
        );"#,
    ).await?;

    conn.execute(
        r#"CREATE TABLE IF NOT EXISTS order_items (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            order_id TEXT NOT NULL,
            product_id INTEGER NOT NULL,
            quantity INTEGER NOT NULL,
            unit_price_cents INTEGER NOT NULL,
            FOREIGN KEY(order_id) REFERENCES orders(id),
            FOREIGN KEY(product_id) REFERENCES products(id)
        );"#,
    ).await?;

    Ok(())
}

// ---------- Main ----------
#[tokio::main]
async fn main() -> Result<(), anyhow::Error> {
    dotenv().ok();

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    let database_url = std::env::var("DATABASE_URL").unwrap_or_else(|_| "sqlite://ecom.db".into());
    info!("Connecting to database at {}", database_url);

    let pool = SqlitePool::connect(&database_url).await?;
    init_db(&pool).await?;

    let app_state = Arc::new(AppState { pool });

    let cors = CorsLayer::new().allow_origin(Any).allow_methods(Any);

    let app = Router::new()
        .route("/api/v1/products", get(list_products).post(create_product))
        .route("/api/v1/products/:id", get(get_product).put(update_product).delete(delete_product))
        .route("/api/v1/orders", post(create_order))
        .route("/api/v1/orders/:id", get(get_order))
        .with_state(Arc::clone(&app_state))
        .layer(cors);

    let addr = SocketAddr::from(([127, 0, 0, 1], 3000));
    info!("Listening on http://{}", addr);
    axum::Server::bind(&addr)
        .serve(app.into_make_service())
        .await
        .unwrap();

    Ok(())
}

// ----------------------------------------
// Notes:
// - This is a single-file, beginner-friendly snapshot. In real projects split code into modules like
//   `routes`, `handlers`, `models`, `db`, `errors` and add unit/integration tests.
// - To run:
//   1. create a new cargo project: `cargo new ecom-backend --bin`
//   2. paste Cargo.toml deps and save this file to src/main.rs
//   3. (optional) create a `.env` with `DATABASE_URL=sqlite://./ecom.db` and `RUST_LOG=info`
//   4. run: `cargo run`
// - Quick curl examples:
//   * Create a product:
//     curl -X POST http://127.0.0.1:3000/api/v1/products -H "Content-Type: application/json" \
//       -d '{"name":"T-shirt","description":"A comfy tee","price_cents":1999,"stock":100}'
//   * List products: `curl http://127.0.0.1:3000/api/v1/products`
//   * Create an order (use real product ids from list):
//     curl -X POST http://127.0.0.1:3000/api/v1/orders -H "Content-Type: application/json" \
//       -d '{"items":[{"product_id":1,"quantity":2}]}'
// ----------------------------------------
