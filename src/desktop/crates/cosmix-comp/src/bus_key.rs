//! Optional F9 action delivered through the native ABP client, off the input thread.

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BusKeyConfig {
    pub(crate) service: String,
    pub(crate) verb: String,
}

impl BusKeyConfig {
    pub(crate) fn parse(service: String, verb: String) -> Result<Self, String> {
        if !cfg!(feature = "bus") {
            return Err("--f9-bus requires a compositor built with Bus support".into());
        }
        if !(2..=31).contains(&service.len())
            || !service.starts_with(|c: char| c.is_ascii_lowercase())
            || !service
                .bytes()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
        {
            return Err("--f9-bus service must be a local Bus service name".into());
        }
        if verb.is_empty()
            || verb.len() > 128
            || !verb
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-'))
        {
            return Err("--f9-bus verb must contain only letters, digits, '.', '_' or '-'".into());
        }
        Ok(Self { service, verb })
    }
}

#[cfg(feature = "bus")]
pub(crate) struct BusKeyWorker {
    trigger: Option<tokio::sync::mpsc::Sender<()>>,
    shutdown: tokio::sync::watch::Sender<bool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

#[cfg(feature = "bus")]
impl BusKeyWorker {
    pub(crate) fn start(config: BusKeyConfig) -> Result<Self, std::io::Error> {
        let (trigger, mut requests) = tokio::sync::mpsc::channel(1);
        let (shutdown, mut stopped) = tokio::sync::watch::channel(false);
        let url = cosmix_config::client_helpers::resolve_noded_url();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let thread = std::thread::Builder::new().name("cosmix-bus-key".into()).spawn(move || {
            runtime.block_on(async move {
                // Each press is independent: a failed request is never retried later.
                loop {
                    tokio::select! {
                        biased;
                        _ = stopped.changed() => break,
                        request = requests.recv() => if request.is_none() { break; },
                    }
                    let budget = std::time::Duration::from_secs(2);
                    let connected = tokio::select! {
                        biased;
                        _ = stopped.changed() => break,
                        result = tokio::time::timeout(budget, cosmix_client::NodedClient::connect_anonymous(&url)) => result,
                    };
                    match connected {
                        Ok(Ok(client)) => {
                            tokio::select! {
                                biased;
                                _ = stopped.changed() => {},
                                result = tokio::time::timeout(budget, client.call(&config.service, &config.verb, serde_json::json!({}))) => match result {
                                    Ok(Ok(reply)) => tracing::info!(service = %config.service, verb = %config.verb, %reply, "F9 Bus action replied"),
                                    Ok(Err(error)) => tracing::warn!(%error, "F9 Bus action failed"),
                                    Err(_) => tracing::warn!("F9 Bus action timed out; outcome unknown"),
                                },
                            }
                            let _ = tokio::time::timeout(std::time::Duration::from_millis(100), client.close()).await;
                            if *stopped.borrow() { break; }
                        }
                        Ok(Err(error)) => tracing::warn!(%error, "F9 Bus connection failed"),
                        Err(_) => tracing::warn!("F9 Bus connection timed out"),
                    }
                }
            });
        })?;
        Ok(Self {
            trigger: Some(trigger),
            shutdown,
            thread: Some(thread),
        })
    }

    pub(crate) fn trigger(&self) {
        if let Some(trigger) = &self.trigger
            && let Err(error) = trigger.try_send(())
        {
            tracing::warn!(%error, "F9 Bus action queue unavailable; press dropped");
        }
    }
}

#[cfg(feature = "bus")]
impl Drop for BusKeyWorker {
    fn drop(&mut self) {
        let _ = self.shutdown.send(true);
        self.trigger.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
