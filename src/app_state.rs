extern crate rocket;

use crate::form_parameters::FormParameters;
use crate::platform::MyResponse;
use chrono::prelude::*;
use mediawiki::api::Api;
use mysql as my;
use rand::seq::SliceRandom;
use rayon::prelude::*;
use rocket::http::ContentType;
use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::sync::Arc;
use std::sync::Mutex;
use std::{thread, time};

static MAX_CONCURRENT_DB_CONNECTIONS: u64 = 10;
static MYSQL_MAX_CONNECTION_ATTEMPTS: u64 = 15;
static MYSQL_CONNECTION_INITIAL_DELAY_MS: u64 = 100;

pub type DbUserPass = (String, String);

#[derive(Debug, Clone)]
pub struct AppState {
    pub db_pool: Vec<Arc<Mutex<DbUserPass>>>,
    pub config: Value,
    tool_db_mutex: Arc<Mutex<DbUserPass>>,
    threads_running: Arc<Mutex<i64>>,
    shutting_down: Arc<Mutex<bool>>,
    site_matrix: Value,
    main_page: String,
}

impl AppState {
    pub fn new_from_config(config: &Value) -> Self {
        let main_page_path = "./html/index.html";
        let tool_db_access_tuple = (
            config["user"].as_str().unwrap().to_string(),
            config["password"].as_str().unwrap().to_string(),
        );
        let mut ret = Self {
            db_pool: vec![],
            config: config.to_owned(),
            threads_running: Arc::new(Mutex::new(0)),
            shutting_down: Arc::new(Mutex::new(false)),
            site_matrix: AppState::load_site_matrix(),
            tool_db_mutex: Arc::new(Mutex::new(tool_db_access_tuple)),
            main_page: String::from_utf8_lossy(&fs::read(main_page_path).unwrap())
                .parse()
                .unwrap(),
        };

        match config["mysql"].as_array() {
            Some(up_list) => {
                up_list.iter().for_each(|up| {
                    let user = up[0].as_str().unwrap().to_string();
                    let pass = up[1].as_str().unwrap().to_string();
                    let connections = up[2].as_u64().unwrap_or(5);
                    for _connection_num in 1..connections {
                        let tuple = (user.to_owned(), pass.to_owned());
                        ret.db_pool.push(Arc::new(Mutex::new(tuple)));
                    }
                    // Ignore toolname up[3]
                });
            }
            None => {
                for _x in 1..MAX_CONCURRENT_DB_CONNECTIONS {
                    let tuple = (
                        config["user"].as_str().unwrap().to_string(),
                        config["password"].as_str().unwrap().to_string(),
                    );
                    ret.db_pool.push(Arc::new(Mutex::new(tuple)));
                }
            }
        }
        if ret.db_pool.is_empty() {
            panic!("No database access config available");
        }
        ret
    }

    pub fn get_main_page(&self) -> &String {
        &self.main_page
    }

    /// Returns the server and database name for the wiki, as a tuple
    pub fn db_host_and_schema_for_wiki(&self, wiki: &String) -> (String, String) {
        // TESTING
        // ssh magnus@tools-login.wmflabs.org -L 3307:wikidatawiki.analytics.db.svc.eqiad.wmflabs:3306 -N
        let host = match self.config["host"].as_str() {
            Some("127.0.0.1") => "127.0.0.1".to_string(),
            Some(_host) => wiki.to_owned() + ".analytics.db.svc.eqiad.wmflabs",
            None => panic!("No host in config file"),
        };
        let schema = wiki.to_owned() + "_p";
        (host, schema)
    }

    /// Returns the server and database name for the tool db, as a tuple
    pub fn db_host_and_schema_for_tool_db(&self) -> (String, String) {
        // TESTING
        // ssh magnus@tools-login.wmflabs.org -L 3308:tools-db:3306 -N
        let host = self.config["host"].as_str().unwrap().to_string();
        let schema = self.config["schema"].as_str().unwrap().to_string();
        (host, schema)
    }

    /// Returns a random mutex. The mutex value itself contains a user name and password for DB login!
    pub fn get_db_mutex(&self) -> &Arc<Mutex<DbUserPass>> {
        // TODO make sure mutex is available
        // TODO make sure mutex is not poisoned
        &self.db_pool.choose(&mut rand::thread_rng()).unwrap()
    }

    pub fn get_wiki_db_connection(
        &self,
        db_user_pass: &DbUserPass,
        wiki: &String,
    ) -> Option<my::Conn> {
        let mut loops_left = MYSQL_MAX_CONNECTION_ATTEMPTS;
        let mut milliseconds = MYSQL_CONNECTION_INITIAL_DELAY_MS;
        loop {
            let (host, schema) = self.db_host_and_schema_for_wiki(wiki);
            let (user, pass) = db_user_pass;
            let mut builder = my::OptsBuilder::new();
            builder
                .ip_or_hostname(Some(host))
                .db_name(Some(schema))
                .user(Some(user))
                .pass(Some(pass));
            builder.tcp_port(self.config["db_port"].as_u64().unwrap_or(3306) as u16);

            match my::Conn::new(builder) {
                Ok(con) => return Some(con),
                Err(e) => {
                    println!("CONNECTION ERROR: {:?}", e);
                    if loops_left == 0 {
                        break;
                    }
                    loops_left -= 1;
                    let sleep_ms = time::Duration::from_millis(milliseconds);
                    milliseconds *= 2;
                    thread::sleep(sleep_ms);
                }
            }
        }
        None
    }

    pub fn render_error(&self, error: String, _form_parameters: &FormParameters) -> MyResponse {
        // TODO render in proper content format
        return MyResponse {
            s: error.to_string(),
            content_type: ContentType::Plain,
        };
    }

    pub fn get_api_for_wiki(&self, wiki: String) -> Option<Api> {
        // TODO cache url and/or api object?
        let url = self.get_server_url_for_wiki(&wiki)? + "/w/api.php";
        Api::new(&url).ok()
    }

    fn get_value_from_site_matrix_entry(
        &self,
        value: &String,
        site: &Value,
        key_match: &str,
        key_return: &str,
    ) -> Option<String> {
        if site["closed"].as_str().is_some() {
            return None;
        }
        if site["private"].as_str().is_some() {
            return None;
        }
        match site[key_match].as_str() {
            Some(site_url) => {
                if value == site_url {
                    match site[key_return].as_str() {
                        Some(url) => Some(url.to_string()),
                        None => None,
                    }
                } else {
                    None
                }
            }
            None => None,
        }
    }

    fn get_wiki_for_server_url_from_site(&self, url: &String, site: &Value) -> Option<String> {
        self.get_value_from_site_matrix_entry(url, site, "url", "dbname")
    }

    fn get_url_for_wiki_from_site(&self, wiki: &String, site: &Value) -> Option<String> {
        self.get_value_from_site_matrix_entry(wiki, site, "dbname", "url")
    }

    pub fn get_wiki_for_server_url(&self, url: &String) -> Option<String> {
        self.site_matrix["sitematrix"]
            .as_object()
            .expect("AppState::get_wiki_for_server_url: sitematrix not an object")
            .iter()
            .filter_map(|(id, data)| match id.as_str() {
                "count" => None,
                "specials" => data
                    .as_array()
                    .expect("AppState::get_wiki_for_server_url: 'specials' is not an array")
                    .iter()
                    .filter_map(|site| self.get_wiki_for_server_url_from_site(url, site))
                    .next(),
                _other => match data["site"].as_array() {
                    Some(sites) => sites
                        .iter()
                        .filter_map(|site| self.get_wiki_for_server_url_from_site(url, site))
                        .next(),
                    None => None,
                },
            })
            .next()
    }

    pub fn get_server_url_for_wiki(&self, wiki: &String) -> Option<String> {
        self.site_matrix["sitematrix"]
            .as_object()
            .expect("AppState::get_server_url_for_wiki: sitematrix not an object")
            .iter()
            .filter_map(|(id, data)| match id.as_str() {
                "count" => None,
                "specials" => data
                    .as_array()
                    .expect("AppState::get_server_url_for_wiki: 'specials' is not an array")
                    .iter()
                    .filter_map(|site| self.get_url_for_wiki_from_site(wiki, site))
                    .next(),
                _other => match data["site"].as_array() {
                    Some(sites) => sites
                        .iter()
                        .filter_map(|site| self.get_url_for_wiki_from_site(wiki, site))
                        .next(),
                    None => None,
                },
            })
            .next()
    }

    pub fn get_tool_db_connection(
        &self,
        tool_db_user_pass: DbUserPass,
    ) -> Result<my::Conn, String> {
        let (host, schema) = self.db_host_and_schema_for_tool_db();
        let (user, pass) = tool_db_user_pass.clone();
        let mut builder = my::OptsBuilder::new();
        builder
            .ip_or_hostname(Some(host.to_owned()))
            .db_name(Some(schema))
            .user(Some(user))
            .pass(Some(pass));
        let port: u16 = match self.config["host"].as_str() {
            Some("127.0.0.1") => 3308,
            Some(_host) => self.config["db_port"].as_u64().unwrap_or(3306) as u16,
            None => 3306, // Fallback
        };
        builder.tcp_port(port);

        match my::Conn::new(builder) {
            Ok(conn) => Ok(conn),
            Err(e) => Err(format!(
                "AppState::get_tool_db_connection can't get DB connection to {}:{} : '{}'",
                &host, port, &e
            )),
        }
    }

    pub fn get_query_from_psid(&self, psid: &String) -> Result<String, String> {
        let tool_db_user_pass = self.tool_db_mutex.lock().unwrap(); // Force DB connection placeholder
        let mut conn = self.get_tool_db_connection(tool_db_user_pass.clone())?;

        let sql = format!(
            "SELECT querystring FROM query WHERE id={}",
            psid.parse::<usize>().unwrap()
        );
        let result = match conn.prep_exec(sql, ()) {
            Ok(r) => r,
            Err(e) => {
                return Err(format!(
                    "AppState::get_query_from_psid query error: {:?}",
                    e
                ))
            }
        };
        for row in result {
            let query: String = my::from_row(row.unwrap());
            return Ok(query);
        }
        Err("No such PSID in the database".to_string())
    }

    pub fn get_or_create_psid_for_query(&self, query_string: &String) -> Result<u64, String> {
        let tool_db_user_pass = self.tool_db_mutex.lock().unwrap(); // Force DB connection placeholder
        let mut conn = self.get_tool_db_connection(tool_db_user_pass.clone())?;

        // Check for existing entry
        let sql = (
            "SELECT id FROM query WHERE querystring=? LIMIT 1".to_string(),
            vec![query_string.to_owned()],
        );
        match conn.prep_exec(sql.0, sql.1) {
            Ok(result) => {
                for row in result {
                    let psid: u64 = my::from_row(row.unwrap());
                    return Ok(psid);
                }
            }
            Err(_) => {}
        }

        // Create new entry
        let utc: DateTime<Utc> = Utc::now();
        let now = utc.format("%Y-%m-%d- %H:%M:%S").to_string();
        let sql = (
            "INSERT IGNORE INTO `query` (querystring,created) VALUES (?,?)".to_string(),
            vec![query_string.to_owned(), now],
        );
        let ret = match conn.prep_exec(sql.0, sql.1) {
            Ok(r) => Ok(r.last_insert_id()),
            Err(e) => Err(format!(
                "AppState::get_new_psid_for_query query error: {:?}",
                e
            )),
        };
        ret
    }

    fn load_site_matrix() -> Value {
        let api =
            Api::new("https://www.wikidata.org/w/api.php").expect("Can't talk to Wikidata API");
        let params: HashMap<String, String> = vec![("action", "sitematrix")]
            .par_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        api.get_query_api_json(&params)
            .expect("Can't run action=sitematrix on Wikidata API")
    }

    pub fn modify_threads_running(&self, diff: i64) {
        let mut threads_running = self.threads_running.lock().unwrap();
        *threads_running += diff;
        if self.is_shutting_down() && *threads_running == 0 {
            panic!("Planned shutdown")
        }
    }

    pub fn is_shutting_down(&self) -> bool {
        *self.shutting_down.lock().unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    //use crate::app_state::AppState;
    use serde_json::Value;
    use std::env;
    use std::fs::File;

    fn get_new_state() -> Arc<AppState> {
        let basedir = env::current_dir()
            .expect("Can't get CWD")
            .to_str()
            .unwrap()
            .to_string();
        let path = basedir.to_owned() + "/config.json";
        let file = File::open(path).expect("Can not open config file");
        let petscan_config: Value =
            serde_json::from_reader(file).expect("Can not parse JSON from config file");
        Arc::new(AppState::new_from_config(&petscan_config))
    }

    fn get_state() -> Arc<AppState> {
        lazy_static! {
            static ref STATE: Arc<AppState> = get_new_state();
        }
        STATE.clone()
    }

    #[test]
    fn test_get_wiki_for_server_url() {
        let state = get_state();
        assert_eq!(
            state.get_wiki_for_server_url(&"https://am.wiktionary.org".to_string()),
            Some("amwiktionary".to_string())
        );
        assert_eq!(
            state.get_wiki_for_server_url(&"https://outreach.wikimedia.org".to_string()),
            Some("outreachwiki".to_string())
        );
    }
}
