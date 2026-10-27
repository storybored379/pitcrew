//! One-file stack configuration adapter. Legacy JSON runtime remains supported.
use crate::runtime::{
    DiscordConfig, RuntimeConfig, SeerrConfig, TelegramConfig, validate_channels,
};
use pitcrew_config::{ChannelAdapter, Config};

pub fn runtime(config: &Config) -> Result<RuntimeConfig, &'static str> {
    let plex = config
        .plex
        .as_ref()
        .ok_or("bot requires plex configuration")?;
    let mut result = RuntimeConfig {
        database: config.stack.intake_database.clone(),
        catalog: config
            .stack
            .catalog
            .iter()
            .map(|item| pitcrew_intake::CatalogItem {
                id: item.id.clone(),
                title: item.title.clone(),
                aliases: item.aliases.clone(),
                library_id: item.library_id.clone(),
                library_name: item.library_name.clone(),
                resolution: match item.resolution {
                    pitcrew_config::StackResolution::Hd => pitcrew_intake::Resolution::Hd,
                    pitcrew_config::StackResolution::Uhd => pitcrew_intake::Resolution::Uhd,
                },
                season: item.season,
                episode: item.episode,
            })
            .collect(),
        plex_auth_url: config.stack.plex_auth_url.clone(),
        plex_server_url: plex.url.clone(),
        plex_server_id: plex.server_id.clone(),
        client_id: config.stack.plex_client_id.clone(),
        discord: None,
        telegram: None,
        seerr: None,
    };
    for channel in &config.channels {
        match &channel.adapter {
            ChannelAdapter::Discord { token, .. } => {
                if result.discord.is_some() {
                    return Err("bot supports one Discord channel instance");
                }
                result.discord = Some(DiscordConfig {
                    channel_id: channel.id.clone(),
                    bot_token: token
                        .resolve("channels.adapter.token")
                        .map_err(|_| "bot token source unavailable")?
                        .expose()
                        .into(),
                });
            }
            ChannelAdapter::Telegram { token, .. } => {
                if result.telegram.is_some() {
                    return Err("bot supports one Telegram channel instance");
                }
                result.telegram = Some(TelegramConfig {
                    channel_id: channel.id.clone(),
                    bot_token: token
                        .resolve("channels.adapter.token")
                        .map_err(|_| "bot token source unavailable")?
                        .expose()
                        .into(),
                });
            }
            ChannelAdapter::Seerr { url, api_key } => {
                if result.seerr.is_some() {
                    return Err("bot supports one Seerr channel instance");
                }
                result.seerr = Some(SeerrConfig {
                    channel_id: channel.id.clone(),
                    api_url: url.clone(),
                    api_key: api_key
                        .resolve("channels.adapter.api_key")
                        .map_err(|_| "bot API key source unavailable")?
                        .expose()
                        .into(),
                    bot_user_id: config
                        .stack
                        .bot_seerr_user_id
                        .ok_or("Seerr requires stack.bot_seerr_user_id")?,
                });
            }
            ChannelAdapter::Web { .. } => {}
            ChannelAdapter::Email { .. } | ChannelAdapter::Matrix { .. } => {
                return Err("configured bot adapter has no implemented runtime");
            }
        }
    }
    validate_channels(&result)?;
    Ok(result)
}
