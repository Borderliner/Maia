//! A small client for a local Ollama (http://127.0.0.1:11434, or OLLAMA_HOST), used by
//! search by meaning: is it running, is the model there, fetch it, embed text. Plain HTTP
//! over a TCP socket, enough for Ollama's JSON API and its chunked streams.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

/// Google's EmbeddingGemma 2 (Google's own build, from Hugging Face): one meaning space
/// for text and pictures, multilingual, made for search on one's own files.
pub const MODEL: &str = "hf.co/ggml-org/embeddinggemma-2-GGUF:Q8_0";
/// Fingerprint length: what Ollama returns for this build (Matryoshka: the first numbers
/// carry the most, so a longer answer is cut to this).
pub const DIMS: usize = 512;

/// CPU threads Ollama may use for this (it otherwise takes every core).
const THREADS: usize = 4;

#[cfg(test)]
pub static TEST_HOST: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// host:port of the Ollama to use.
pub fn host() -> String {
    #[cfg(test)]
    if let Some(h) = TEST_HOST.lock().unwrap_or_else(|e| e.into_inner()).clone() {
        return h;
    }
    let v = std::env::var("OLLAMA_HOST").unwrap_or_default();
    let v = v.trim().trim_start_matches("http://").trim_end_matches('/');
    match v.rsplit_once(':') {
        _ if v.is_empty() => "127.0.0.1:11434".into(),
        Some(("", port)) => format!("127.0.0.1:{port}"),
        Some(_) => v.to_string(),
        None => format!("{v}:11434"),
    }
}

/// An HTTP response whose body is read as it arrives.
struct Response {
    status: u16,
    body: Box<dyn BufRead>,
}

fn request(method: &str, path: &str, body: Option<&str>, timeout: Duration) -> Result<Response, String> {
    let host = host();
    let addr = host.to_socket_addrs().ok().and_then(|mut a| a.next()).ok_or_else(|| format!("No address for {host}"))?;
    let mut s = TcpStream::connect_timeout(&addr, Duration::from_millis(800)).map_err(|_| "Ollama isn't running".to_string())?;
    let _ = s.set_read_timeout(Some(timeout));
    let _ = s.set_write_timeout(Some(Duration::from_secs(10)));
    let body = body.unwrap_or("");
    let head = format!("{method} {path} HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
    s.write_all(head.as_bytes()).and_then(|_| s.write_all(body.as_bytes())).map_err(|e| e.to_string())?;
    let mut r = BufReader::new(s);
    let mut line = String::new();
    r.read_line(&mut line).map_err(|e| e.to_string())?;
    let status = line.split_whitespace().nth(1).and_then(|c| c.parse().ok()).ok_or("Not an HTTP answer")?;
    let (mut chunked, mut len) = (false, None);
    loop {
        line.clear();
        if r.read_line(&mut line).map_err(|e| e.to_string())? == 0 || line.trim().is_empty() {
            break;
        }
        let (k, v) = line.split_once(':').unwrap_or((&line, ""));
        match k.trim().to_ascii_lowercase().as_str() {
            "transfer-encoding" => chunked = v.to_ascii_lowercase().contains("chunked"),
            "content-length" => len = v.trim().parse::<u64>().ok(),
            _ => {}
        }
    }
    let body: Box<dyn BufRead> = if chunked {
        Box::new(BufReader::new(Chunked { r, left: 0, done: false }))
    } else {
        Box::new(BufReader::new(r.take(len.unwrap_or(u64::MAX))))
    };
    Ok(Response { status, body })
}

/// A chunked HTTP body as plain bytes.
struct Chunked<R: BufRead> {
    r: R,
    left: usize,
    done: bool,
}

impl<R: BufRead> Read for Chunked<R> {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        if self.done {
            return Ok(0);
        }
        if self.left == 0 {
            let mut line = String::new();
            self.r.read_line(&mut line)?;
            if line.trim().is_empty() {
                // The blank line closing the previous chunk.
                line.clear();
                self.r.read_line(&mut line)?;
            }
            self.left = usize::from_str_radix(line.trim().split(';').next().unwrap_or("0"), 16).unwrap_or(0);
            if self.left == 0 {
                self.done = true;
                return Ok(0);
            }
        }
        let want = out.len().min(self.left);
        let n = self.r.read(&mut out[..want])?;
        self.left -= n;
        if n == 0 {
            self.done = true;
        }
        Ok(n)
    }
}

fn json(r: Response) -> Result<serde_json::Value, String> {
    let ok = (200..300).contains(&r.status);
    let mut text = String::new();
    let mut body = r.body;
    body.read_to_string(&mut text).map_err(|e| e.to_string())?;
    let v: serde_json::Value = serde_json::from_str(&text).map_err(|_| format!("Ollama answered {}", r.status))?;
    if !ok {
        return Err(v["error"].as_str().unwrap_or("Ollama refused").to_string());
    }
    Ok(v)
}

#[derive(Clone, Debug, PartialEq)]
pub enum Status {
    NotRunning,
    NoModel,
    Ready,
}

pub fn status() -> Status {
    let Ok(r) = request("GET", "/api/tags", None, Duration::from_secs(5)) else { return Status::NotRunning };
    let Ok(v) = json(r) else { return Status::NotRunning };
    let has = v["models"].as_array().is_some_and(|m| m.iter().any(|m| m["name"].as_str().is_some_and(|n| n.eq_ignore_ascii_case(MODEL))));
    if has { Status::Ready } else { Status::NoModel }
}

/// Download the model, reporting (bytes done, bytes in all) as it goes.
pub fn pull(mut progress: impl FnMut(u64, u64), stop: &std::sync::atomic::AtomicBool) -> Result<(), String> {
    let body = serde_json::json!({ "model": MODEL }).to_string();
    let r = request("POST", "/api/pull", Some(&body), Duration::from_secs(120))?;
    if !(200..300).contains(&r.status) {
        return json(r).map(|_| ());
    }
    for line in r.body.lines() {
        if stop.load(std::sync::atomic::Ordering::Relaxed) {
            return Err("Stopped".into());
        }
        let line = line.map_err(|e| e.to_string())?;
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else { continue };
        if let Some(e) = v["error"].as_str() {
            return Err(e.to_string());
        }
        if let (Some(done), Some(all)) = (v["completed"].as_u64(), v["total"].as_u64()) {
            progress(done, all);
        }
        if v["status"].as_str() == Some("success") {
            return Ok(());
        }
    }
    Err("The download stopped".into())
}

/// Meaning fingerprints for `texts`: DIMS numbers each, of length 1.
pub fn embed(texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
    embed_inputs(texts.iter().map(|t| serde_json::Value::from(t.as_str())).collect())
}

/// Fingerprints for pictures (PNG or JPEG bytes), in the same space as text.
pub fn embed_images(images: &[Vec<u8>]) -> Result<Vec<Vec<f32>>, String> {
    embed_inputs(images.iter().map(|b| serde_json::json!({ "image": base64(b) })).collect())
}

fn base64(b: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut s = String::with_capacity(b.len().div_ceil(3) * 4);
    for c in b.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        for k in 0..4 {
            s.push(if k <= c.len() { T[(n >> (18 - 6 * k) & 63) as usize] as char } else { '=' });
        }
    }
    s
}

fn embed_inputs(inputs: Vec<serde_json::Value>) -> Result<Vec<Vec<f32>>, String> {
    let n = inputs.len();
    // A few threads, not all of them: indexing is background work, and the rest of the
    // machine stays free (pictures take a little longer, about 3 s each on a laptop).
    let body = serde_json::json!({ "model": MODEL, "input": inputs, "truncate": true, "keep_alive": "2m", "options": { "num_thread": THREADS } }).to_string();
    let v = json(request("POST", "/api/embed", Some(&body), Duration::from_secs(180))?)?;
    let out: Vec<Vec<f32>> = v["embeddings"].as_array().ok_or("No embeddings in the answer")?.iter().map(|e| e.as_array().map(|a| a.iter().take(DIMS).map(|x| x.as_f64().unwrap_or(0.0) as f32).collect()).unwrap_or_default()).collect();
    if out.len() != n || out.iter().any(|e| e.len() < DIMS) {
        return Err("Ollama gave fingerprints of the wrong shape".into());
    }
    // Cut to DIMS (Matryoshka) and scaled back to length 1.
    Ok(out.into_iter().map(|mut e| {
        let n = e.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-9);
        e.iter_mut().for_each(|x| *x /= n);
        e
    }).collect())
}
