use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use steamid_ng::SteamID;
use tappet::{
    response_types::{
        GetFriendListResponseBase, GetPlayerBansResponseBase, GetPlayerSummariesResponseBase,
        PlayerBans, PlayerSummary,
    },
    Executor, SteamAPI,
};

use tokio::sync::mpsc::unbounded_channel;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::mpsc::UnboundedSender;
use tokio::time::{Duration, MissedTickBehavior};

use crate::player::{Friend, SteamInfo};

const BATCH_INTERVAL: Duration = Duration::from_millis(500);
const BATCH_SIZE: usize = 20; // adjust as needed

#[derive(Clone, Debug)]
pub enum SteamAPIMessage {
    Lookup(SteamID),
    CheckFriends(Vec<SteamID>),
    SetAPIKey(Arc<str>),
}

pub enum SteamAPIResponse {
    SteamInfo((SteamID, SteamInfo)),
    FriendLists((SteamID, Result<Vec<Friend>>)),
}

pub struct SteamAPIManager {
    client: SteamAPI,
    batch_buffer: VecDeque<SteamID>,
    api_key_valid: bool,

    request_recv: UnboundedReceiver<SteamAPIMessage>,
    response_send: UnboundedSender<SteamAPIResponse>,
}

impl SteamAPIManager {
    pub fn new(
        api_key: Arc<str>,
        recv: UnboundedReceiver<SteamAPIMessage>,
    ) -> (UnboundedReceiver<SteamAPIResponse>, SteamAPIManager) {
        let (resp_tx, resp_rx) = unbounded_channel();

        let valid_api_key = is_api_key_valid(&api_key);
        if !valid_api_key {
            tracing::info!("Invalid/Improper API key provided, disabling Steam API requests.");
        }

        let api_manager = SteamAPIManager {
            client: SteamAPI::new(api_key),
            batch_buffer: VecDeque::with_capacity(BATCH_SIZE),
            api_key_valid: valid_api_key,

            request_recv: recv,
            response_send: resp_tx,
        };

        (resp_rx, api_manager)
    }

    fn set_api_key(&mut self, api_key: Arc<str>) {
        let _last = self.api_key_valid;
        self.api_key_valid = is_api_key_valid(&api_key);
        self.client = SteamAPI::new(api_key);
        if !_last && self.api_key_valid {
            tracing::info!("New API key received, enabling SteamAPI requests.");
        } else if _last && !self.api_key_valid {
            tracing::info!("Invalid/Improper API key received, disabling SteamAPI requests.");
        } else {
            tracing::info!("Updated SteamAPI key.");
        }
    }

    /// Enter a loop to wait for steam lookup requests, make those requests from the Steam web API,
    /// and update the state to include that data. Intended to be run inside a new tokio::task
    pub async fn api_loop(&mut self) {
        let mut batch_timer = tokio::time::interval(BATCH_INTERVAL);
        batch_timer.set_missed_tick_behavior(MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                Some(request) = self.request_recv.recv() => {
                    match request {
                        SteamAPIMessage::SetAPIKey(key) => {
                            self.set_api_key(key);
                        },
                        SteamAPIMessage::Lookup(steamid) => {
                            if self.api_key_valid {
                                self.batch_buffer.push_back(steamid);
                                if self.batch_buffer.len() >= BATCH_SIZE {
                                    self.send_batch().await;
                                    batch_timer.reset();  // Reset the timer
                                }
                            }
                        },
                        SteamAPIMessage::CheckFriends(steamids) => {
                            if self.api_key_valid {
                                for id in steamids {
                                    match request_account_friends(&mut self.client, id).await {
                                        Ok(friends) => {
                                            self.response_send
                                                .send(SteamAPIResponse::FriendLists((id, Ok(friends))))
                                                .expect("Lost connection to main thread.");
                                        }
                                        Err(err) => {
                                            self.response_send
                                                .send(SteamAPIResponse::FriendLists((id, Err(err))))
                                                .expect("Lost connection to main thread.");
                                        }
                                    }
                                }
                            }
                        }
                    }
                },
                _ = batch_timer.tick() => {
                    if self.api_key_valid && !self.batch_buffer.is_empty() {
                        self.send_batch().await;
                    }
                }
            }
        }
    }

    async fn send_batch(&mut self) {
        match request_steam_info(&mut self.client, self.batch_buffer.drain(..).collect()).await {
            Ok(steam_info_map) => {
                for response in steam_info_map {
                    self.response_send
                        .send(SteamAPIResponse::SteamInfo(response))
                        .expect("Lost connection to main thread.");
                }
            }
            Err(e) => {
                tracing::error!("Failed to get player info from SteamAPI: {:?}", e);
            }
        }
    }
}

/// Make a request to the Steam web API for the chosen player and return the important steam info.
async fn request_steam_info(
    client: &mut SteamAPI,
    playerids: Vec<SteamID>,
) -> Result<Vec<(SteamID, SteamInfo)>> {
    tracing::debug!("Requesting steam accounts: {:?}", playerids);

    let summaries = request_player_summary(client, &playerids).await?;
    let bans = request_account_bans(client, &playerids).await?;

    let id_to_summary: HashMap<_, _> = summaries
        .into_iter()
        .map(|summary| (summary.steamid.clone(), summary))
        .collect();
    let id_to_ban: HashMap<_, _> = bans
        .into_iter()
        .map(|ban| (ban.steam_id.clone(), ban))
        .collect();

    let steam_infos = playerids
        .into_iter()
        .map(|player| {
            let id = format!("{}", u64::from(player));
            let summary = id_to_summary
                .get(&id)
                .ok_or(anyhow!("Missing summary for player {}", id))?;
            let ban = id_to_ban
                .get(&id)
                .ok_or(anyhow!("Missing ban info for player {}", id))?;
            let steam_info = SteamInfo {
                account_name: summary.personaname.clone().into(),
                pfp_url: summary.avatarfull.clone().into(),
                profile_url: summary.profileurl.clone().into(),
                pfp_hash: summary.avatarhash.clone().into(),
                profile_visibility: summary.communityvisibilitystate.into(),
                time_created: summary.timecreated,
                country_code: summary.loccountrycode.clone().map(|s| s.into()),
                vac_bans: ban.number_of_vac_bans,
                game_bans: ban.number_of_game_bans,
                days_since_last_ban: if ban.number_of_vac_bans > 0 || ban.number_of_game_bans > 0 {
                    Some(ban.days_since_last_ban)
                } else {
                    None
                },
            };
            Ok((player, steam_info))
        })
        .collect::<Result<_>>()?;

    Ok(steam_infos)
}

async fn request_player_summary(
    client: &mut SteamAPI,
    players: &[SteamID],
) -> Result<Vec<PlayerSummary>> {
    let summaries = client
        .get()
        .ISteamUser()
        .GetPlayerSummaries(
            players
                .iter()
                .map(|player| format!("{}", u64::from(*player)))
                .collect(),
        )
        .execute()
        .await
        .context("Failed to get player summary from SteamAPI.")?;
    let summaries = serde_json::from_str::<GetPlayerSummariesResponseBase>(&summaries)
        .with_context(|| {
            format!(
                "Failed to parse player summary from SteamAPI: {}",
                &summaries
            )
        })?;
    Ok(summaries.response.players)
}

pub async fn request_account_friends(
    client: &mut SteamAPI,
    player: SteamID,
) -> Result<Vec<Friend>> {
    let friends = client
        .get()
        .ISteamUser()
        .GetFriendList(player.into(), "all".to_string())
        .execute()
        .await
        .context("Failed to get account friends from SteamAPI, profile may be private.")?;
    let friends =
        serde_json::from_str::<GetFriendListResponseBase>(&friends).with_context(|| {
            format!(
                "Failed to parse account friends from SteamAPI: {}",
                &friends
            )
        })?;
    Ok(friends
        .friendslist
        .map(|fl| fl.friends)
        .unwrap_or(Vec::new())
        .iter()
        .filter_map(|f| match f.steamid.parse::<u64>() {
            Err(_) => None,
            Ok(id) => Some(Friend {
                steamid: SteamID::from(id),
                friend_since: f.friend_since,
            }),
        })
        .collect())
}

async fn request_account_bans(
    client: &mut SteamAPI,
    players: &[SteamID],
) -> Result<Vec<PlayerBans>> {
    let bans = client
        .get()
        .ISteamUser()
        .GetPlayerBans(
            players
                .iter()
                .map(|player| format!("{}", u64::from(*player)))
                .collect(),
        )
        .execute()
        .await
        .context("Failed to get player bans from SteamAPI")?;
    let bans = serde_json::from_str::<GetPlayerBansResponseBase>(&bans)
        .with_context(|| format!("Failed to parse player bans from SteamAPI: {}", &bans))?;
    Ok(bans.players)
}

fn is_api_key_valid(api_key: &Arc<str>) -> bool {
    // A valid steam API key is a 32 digit hexadecimal number. We store them as strings, so
    // we check for exactly 32 hexadecimal ascii digits. Anything that doesn't fit this rule
    // is likely not a valid Steam API key (inb4 Valve changes the format on my ass)
    return api_key.len() == 32
        && api_key
            .chars()
            .map(|c| c.is_ascii_hexdigit())
            .reduce(|acc, e| acc && e)
            .unwrap();
}
