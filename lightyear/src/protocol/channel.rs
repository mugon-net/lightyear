use crate::_internal::{
    EntityActionsChannel, EntityUpdatesChannel, InputChannel, PingChannel, TickBufferChannel,
};
use bevy::app::App;
use bevy::ecs::entity::MapEntities;
use bevy::prelude::{Resource, TypePath};
use bevy::reflect::Reflect;
use serde::Deserialize;
use std::any::TypeId;
use std::collections::HashMap;

use crate::channel::builder::ChannelContainer;
use crate::channel::builder::{Channel, ChannelBuilder, ChannelSettings};
use crate::prelude::{
    ChannelDirection, ChannelMode, DefaultUnorderedUnreliableChannel, Message, ReliableSettings,
};
use crate::protocol::registry::{NetId, TypeKind, TypeMapper};

// TODO: derive Reflect once we reach bevy 0.14
/// ChannelKind - internal wrapper around the type of the channel
#[derive(Debug, Eq, Hash, Copy, Clone, PartialEq)]
pub struct ChannelKind(TypeId);

pub type ChannelId = NetId;

impl ChannelKind {
    pub fn of<C: Channel>() -> Self {
        Self(TypeId::of::<C>())
    }
}

impl TypeKind for ChannelKind {}

impl From<TypeId> for ChannelKind {
    fn from(type_id: TypeId) -> Self {
        Self(type_id)
    }
}

/// Registry to store metadata about the various [`Channel`]
#[derive(Resource, Clone, Debug, PartialEq, TypePath)]
pub struct ChannelRegistry {
    // we only store the ChannelBuilder because we might want to create multiple instances of the same channel
    pub(in crate::protocol) builder_map: HashMap<ChannelKind, ChannelBuilder>,
    pub(in crate::protocol) kind_map: TypeMapper<ChannelKind>,
    pub(in crate::protocol) name_map: HashMap<ChannelKind, String>,
    built: bool,
}

impl Default for ChannelRegistry {
    fn default() -> Self {
        let mut registry = Self {
            builder_map: HashMap::new(),
            kind_map: TypeMapper::new(),
            name_map: HashMap::new(),
            built: false,
        };
        registry.add_channel::<EntityActionsChannel>(ChannelSettings {
            mode: ChannelMode::UnorderedReliable(ReliableSettings::default()),
            direction: ChannelDirection::Bidirectional,
            // we want to send the entity actions as soon as possible
            priority: 10.0,
        });
        registry.add_channel::<EntityUpdatesChannel>(ChannelSettings {
            mode: ChannelMode::UnorderedUnreliableWithAcks,
            direction: ChannelDirection::Bidirectional,
            priority: 1.0,
        });
        registry.add_channel::<PingChannel>(ChannelSettings {
            mode: ChannelMode::SequencedUnreliable,
            direction: ChannelDirection::Bidirectional,
            // we always want to include the ping in the packet
            priority: 1000.0,
        });
        registry.add_channel::<InputChannel>(ChannelSettings {
            mode: ChannelMode::UnorderedUnreliable,
            direction: ChannelDirection::ClientToServer,
            priority: 3.0,
        });
        registry.add_channel::<DefaultUnorderedUnreliableChannel>(ChannelSettings {
            mode: ChannelMode::UnorderedUnreliable,
            direction: ChannelDirection::Bidirectional,
            priority: 1.0,
        });
        registry.add_channel::<TickBufferChannel>(ChannelSettings {
            mode: ChannelMode::TickBuffered,
            direction: ChannelDirection::ClientToServer,
            priority: 1.0,
        });
        registry
    }
}

impl ChannelRegistry {
    /// Build all the channels in the registry
    pub fn channels(&self) -> HashMap<ChannelKind, ChannelContainer> {
        let mut channels = HashMap::new();
        for (type_id, builder) in self.builder_map.iter() {
            channels.insert(*type_id, builder.build());
        }
        channels
    }

    pub fn kind_map(&self) -> TypeMapper<ChannelKind> {
        self.kind_map.clone()
    }

    /// Register a new type
    pub fn add_channel<C: Channel>(&mut self, settings: ChannelSettings) {
        let kind = self.kind_map.add::<C>();
        self.builder_map.insert(kind, C::get_builder(settings));
        let name = C::name();
        self.name_map.insert(kind, name.to_string());
    }

    /// get the registered object for a given type
    pub fn get_builder_from_kind(&self, channel_kind: &ChannelKind) -> Option<&ChannelBuilder> {
        self.builder_map.get(channel_kind)
    }

    pub fn get_kind_from_net_id(&self, channel_id: ChannelId) -> Option<&ChannelKind> {
        self.kind_map.kind(channel_id)
    }

    pub fn get_net_from_kind(&self, kind: &ChannelKind) -> Option<&NetId> {
        self.kind_map.net_id(kind)
    }

    pub fn name(&self, kind: &ChannelKind) -> Option<&str> {
        self.name_map.get(kind).map(|s| s.as_str())
    }

    pub fn get_builder_from_net_id(&self, channel_id: ChannelId) -> Option<&ChannelBuilder> {
        let channel_kind = self.get_kind_from_net_id(channel_id)?;
        self.get_builder_from_kind(channel_kind)
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.kind_map.len()
    }
}

/// Add a message to the list of messages that can be sent
pub trait AppChannelExt {
    fn add_channel<C: Channel>(&mut self, settings: ChannelSettings);
}

impl AppChannelExt for App {
    fn add_channel<C: Channel>(&mut self, settings: ChannelSettings) {
        let mut registry = self.world.resource_mut::<ChannelRegistry>();
        registry.add_channel::<C>(settings);
    }
}

#[cfg(test)]
mod tests {
    use bevy::prelude::{default, TypePath};
    use lightyear_macros::ChannelInternal;

    use crate::channel::builder::{ChannelDirection, ChannelMode, ChannelSettings};

    use super::*;

    #[derive(ChannelInternal, TypePath)]
    pub struct MyChannel;

    #[test]
    fn test_channel_registry() -> anyhow::Result<()> {
        let mut registry = ChannelRegistry::new();

        let settings = ChannelSettings {
            mode: ChannelMode::UnorderedUnreliable,
            ..default()
        };
        registry.add_channel::<MyChannel>(settings.clone());
        assert_eq!(registry.len(), 1);

        let builder = registry.get_builder_from_net_id(0).unwrap();
        let channel_container: ChannelContainer = builder.build();
        assert_eq!(
            channel_container.setting.mode,
            ChannelMode::UnorderedUnreliable
        );
        Ok(())
    }
}
