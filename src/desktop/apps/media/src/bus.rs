use crate::player::{self, Player, Request};
use cosmix_client::{BoundedIncomingEvent, SupervisedClient};
use serde_json::json;
use std::{sync::atomic::Ordering, time::Duration};

pub fn start(player: &Player, service: String) {
    let sender = player.sender.clone();
    let shared = player.shared.clone();
    let quit = player.quit.clone();
    std::thread::Builder::new().name("media-bus".into()).spawn(move || {
        let runtime=tokio::runtime::Builder::new_current_thread().enable_all().build().expect("Bus runtime");
        runtime.block_on(async move {
            let connection=SupervisedClient::connect_options(&service,&cosmix_config::client_helpers::resolve_noded_url()).bounded_incoming(32).connect().await;
            let client=match connection {Ok(c)=>c,Err(e)=>{eprintln!("media Bus unavailable: {e}");return;}};
            let Some(mut incoming)=client.incoming_bounded() else{return};
            while !quit.load(Ordering::Relaxed){
                tokio::select!{
                    _=tokio::time::sleep(Duration::from_millis(100))=>{},
                    event=incoming.recv()=>{
                        let command = match event {
                            Some(BoundedIncomingEvent::Command(command)) => command,
                            Some(BoundedIncomingEvent::Overflow { .. }) => continue,
                            None => break,
                        };
                        let result=if command.body.len()>8192 {Err("request exceeds 8192 bytes".into())} else if command.command=="media.status" || command.command=="media.props.get" {
                            let snapshot=shared.lock().unwrap().status.value();
                            let args=if command.body.trim().is_empty(){Ok(json!({}))}else{serde_json::from_str(&command.body)};
                            match args {
                                Ok(args) if args.as_object().is_some_and(|m|m.keys().all(|k|k=="path"))=>{
                                    if let Some(path)=args.get("path") {match path.as_str(){Some("")=>Ok(snapshot),Some(key)=>snapshot.get(key).cloned().ok_or("unknown property".into()),None=>Err("path must be a string".into())}}else{Ok(snapshot)}
                                },
                                _=>Err("expected an object with optional path".into())
                            }
                        }else{
                            match player::parse(&command.command,&command.body){
                                Err(e)=>Err(e),Ok(action)=>{
                                    let (tx,rx)=tokio::sync::oneshot::channel();
                                    if sender.try_send(Request{action,reply:Some(tx)}).is_err(){Err("playback queue unavailable".into())}
                                    else{match tokio::time::timeout(Duration::from_secs(5),rx).await {Ok(Ok(result))=>result,_=>Err("operation pending or worker unavailable; read media.status".into())}}
                                }
                            }
                        };
                        let (rc,value)=match result{Ok(v)=>(0,v),Err(e)=>(10,json!({"error":e}))};
                        let _=tokio::time::timeout(Duration::from_secs(2),client.respond(&command,rc,&value.to_string())).await;
                    }
                }
            }
            client.close().await;
        });
    }).expect("Bus thread");
}
