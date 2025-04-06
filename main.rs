use actix_files::Files;
use actix_web::{web, App, HttpRequest, HttpResponse, HttpServer, Result};
use chrono::prelude::*;
use parking_lot::RwLock;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;
use tera::{Context, Tera};
use thiserror::Error;

// Define our news sources
const NEWS_SOURCES: &[(&str, &str)] = &[
    ("CoinDesk", "https://www.coindesk.com/arc/outboundfeeds/rss/"),
    ("CryptoSlate", "https://cryptoslate.com/feed/"),
    ("Cointelegraph", "https://cointelegraph.com/rss"),
];

// How frequently to refresh news (in seconds)
const REFRESH_INTERVAL: u64 = 300; // 5 minutes

// Error types
#[derive(Error, Debug)]
enum AppError {
    #[error("HTTP request error: {0}")]
    RequestError(#[from] reqwest::Error),
    
    #[error("RSS parsing error: {0}")]
    ParsingError(String),
}

// News article model
#[derive(Debug, Clone, Serialize, Deserialize)]
struct NewsArticle {
    title: String,
    link: String,
    description: String,
    source: String,
    pub_date: String,
    timestamp: i64, // For sorting
}

impl NewsArticle {
    // Method to check if article contains search term (case insensitive)
    fn matches_search(&self, search_term: &str) -> bool {
        if search_term.is_empty() {
            return true;
        }
        
        let search_lower = search_term.to_lowercase();
        
        self.title.to_lowercase().contains(&search_lower) ||
        self.description.to_lowercase().contains(&search_lower) ||
        self.source.to_lowercase().contains(&search_lower)
    }
}

// Query parameters for search
#[derive(Deserialize)]
struct SearchQuery {
    q: Option<String>,
}

// Application state
struct AppState {
    news_cache: RwLock<Vec<NewsArticle>>,
    templates: Tera,
}

// Helper to extract content from RSS XML
fn extract_text(xml: &str, tag: &str) -> Result<String, AppError> {
    let start_tag = format!("<{}>", tag);
    let end_tag = format!("</{}>", tag);
    
    match (xml.find(&start_tag), xml.find(&end_tag)) {
        (Some(start), Some(end)) => {
            let start_pos = start + start_tag.len();
            if start_pos < end {
                Ok(xml[start_pos..end].to_string())
            } else {
                Err(AppError::ParsingError(format!("Invalid tag positions for {}", tag)))
            }
        }
        _ => Err(AppError::ParsingError(format!("Tag not found: {}", tag))),
    }
}

// Parse RSS feed
async fn parse_rss(client: &Client, source_name: &str, url: &str) -> Result<Vec<NewsArticle>, AppError> {
    let response = client.get(url).send().await?.text().await?;
    
    let mut articles = Vec::new();
    
    // Very basic RSS parser - for production use a proper RSS parser crate
    let items: Vec<&str> = response.split("<item>").skip(1).collect();
    
    for item in items {
        if let (Ok(title), Ok(link), Ok(description), Ok(pub_date)) = (
            extract_text(item, "title"),
            extract_text(item, "link"),
            extract_text(item, "description"),
            extract_text(item, "pubDate"),
        ) {
            // Parse the date
            let timestamp = match DateTime::parse_from_rfc2822(&pub_date) {
                Ok(dt) => dt.timestamp(),
                Err(_) => Utc::now().timestamp(), // Fallback to current time
            };
            
            let article = NewsArticle {
                title,
                link,
                description: description.chars().take(200).collect::<String>() + "...",
                source: source_name.to_string(),
                pub_date,
                timestamp,
            };
            
            articles.push(article);
        }
    }
    
    Ok(articles)
}

// Fetch news from all sources
async fn fetch_all_news(client: &Client) -> Vec<NewsArticle> {
    let mut all_articles = Vec::new();
    
    for (source_name, url) in NEWS_SOURCES {
        match parse_rss(client, source_name, url).await {
            Ok(mut articles) => all_articles.append(&mut articles),
            Err(e) => eprintln!("Error fetching from {}: {:?}", source_name, e),
        }
    }
    
    // Sort by timestamp (newest first)
    all_articles.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
    
    all_articles
}

// Background task to refresh news
async fn news_refresher(app_state: Arc<AppState>) {
    let client = Client::new();
    
    loop {
        println!("Refreshing news...");
        let articles = fetch_all_news(&client).await;
        
        // Update the cache
        {
            let mut cache = app_state.news_cache.write();
            *cache = articles;
        }
        
        // Wait for the next refresh interval
        tokio::time::sleep(Duration::from_secs(REFRESH_INTERVAL)).await;
    }
}

// Handler for the home page
async fn index(req: HttpRequest, data: web::Data<Arc<AppState>>) -> Result<HttpResponse> {
    // Get the query parameters
    let query = web::Query::<SearchQuery>::from_query(req.query_string()).unwrap_or(web::Query(SearchQuery { q: None }));
    let search_term = query.q.clone().unwrap_or_default();
    
    // Get all articles
    let all_articles = data.news_cache.read().clone();
    
    // Filter articles based on search term
    let filtered_articles: Vec<NewsArticle> = all_articles
        .into_iter()
        .filter(|article| article.matches_search(&search_term))
        .collect();
    
    let mut context = Context::new();
    context.insert("articles", &filtered_articles);
    context.insert("search_term", &search_term);
    context.insert("last_updated", &Utc::now().to_rfc2822());
    context.insert("article_count", &filtered_articles.len());
    context.insert("has_search", &!search_term.is_empty());
    
    let rendered = data.templates.render("index.html", &context)
        .unwrap_or_else(|e| {
            eprintln!("Template error: {}", e);
            "Error rendering template".to_string()
        });
    
    Ok(HttpResponse::Ok().content_type("text/html").body(rendered))
}

// API endpoint to get news as JSON, with search
async fn api_news(req: HttpRequest, data: web::Data<Arc<AppState>>) -> Result<HttpResponse> {
    // Get the query parameters
    let query = web::Query::<SearchQuery>::from_query(req.query_string()).unwrap_or(web::Query(SearchQuery { q: None }));
    let search_term = query.q.clone().unwrap_or_default();
    
    // Get all articles
    let all_articles = data.news_cache.read().clone();
    
    // Filter articles based on search term
    let filtered_articles: Vec<NewsArticle> = all_articles
        .into_iter()
        .filter(|article| article.matches_search(&search_term))
        .collect();
    
    Ok(HttpResponse::Ok().json(filtered_articles))
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    println!("Starting Cryptocurrency News Aggregator...");
    
    // Initialize templates
    let mut tera = Tera::default();
    tera.add_raw_template("index.html", include_str!("../templates/index.html"))
        .expect("Failed to add template");
    
    // Initialize app state
    let app_state = Arc::new(AppState {
        news_cache: RwLock::new(Vec::new()),
        templates: tera,
    });
    
    // Start background refresher
    let refresher_state = app_state.clone();
    tokio::spawn(async move {
        news_refresher(refresher_state).await;
    });
    
    // Start the web server
    HttpServer::new(move || {
        App::new()
            .app_data(web::Data::new(app_state.clone()))
            .service(web::resource("/").to(index))
            .service(web::resource("/api/news").to(api_news))
            .service(Files::new("/static", "./static"))
    })
    .bind("127.0.0.1:8080")?
    .run()
    .await
}