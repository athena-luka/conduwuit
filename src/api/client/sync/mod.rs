mod v3;
mod v4;
mod v5;

use conduwuit::{
	utils::{
		stream::{BroadbandExt, ReadyExt, TryIgnore},
		IterStream,
	},
	PduCount,
};
use futures::{pin_mut, StreamExt};
use ruma::{
	directory::RoomTypeFilter,
	events::TimelineEventType::{
		self, Beacon, CallInvite, PollStart, RoomEncrypted, RoomMessage, Sticker,
	},
	RoomId, UserId,
};

pub(crate) use self::{
	v3::sync_events_route, v4::sync_events_v4_route, v5::sync_events_v5_route,
};
use crate::{service::Services, Error, PduEvent, Result};

pub(crate) const DEFAULT_BUMP_TYPES: &[TimelineEventType; 6] =
	&[CallInvite, PollStart, Beacon, RoomEncrypted, RoomMessage, Sticker];

async fn load_timeline(
	services: &Services,
	sender_user: &UserId,
	room_id: &RoomId,
	roomsincecount: PduCount,
	next_batch: Option<PduCount>,
	limit: usize,
) -> Result<(Vec<(PduCount, PduEvent)>, bool), Error> {
	let last_timeline_count = services
		.rooms
		.timeline
		.last_timeline_count(Some(sender_user), room_id)
		.await?;

	if last_timeline_count <= roomsincecount {
		return Ok((Vec::new(), false));
	}

	let non_timeline_pdus = services
		.rooms
		.timeline
		.pdus_rev(Some(sender_user), room_id, None)
		.ignore_err()
		.ready_skip_while(|&(pducount, _)| pducount > next_batch.unwrap_or_else(PduCount::max))
		.ready_take_while(|&(pducount, _)| pducount > roomsincecount);

	// Take the last events for the timeline
	pin_mut!(non_timeline_pdus);
	let timeline_pdus: Vec<_> = non_timeline_pdus.by_ref().take(limit).collect().await;

	let timeline_pdus: Vec<_> = timeline_pdus.into_iter().rev().collect();

	// They /sync response doesn't always return all messages, so we say the output
	// is limited unless there are events in non_timeline_pdus
	let limited = non_timeline_pdus.next().await.is_some();

	Ok((timeline_pdus, limited))
}

async fn share_encrypted_room(
	services: &Services,
	sender_user: &UserId,
	user_id: &UserId,
	ignore_room: Option<&RoomId>,
) -> bool {
	services
		.rooms
		.state_cache
		.get_shared_rooms(sender_user, user_id)
		.ready_filter(|&room_id| Some(room_id) != ignore_room)
		.broad_any(|other_room_id| {
			services
				.rooms
				.state_accessor
				.is_encrypted_room(other_room_id)
		})
		.await
}

pub(crate) async fn filter_rooms<'a>(
	services: &Services,
	rooms: &[&'a RoomId],
	filter: &[RoomTypeFilter],
	negate: bool,
) -> Vec<&'a RoomId> {
	rooms
		.iter()
		.stream()
		.filter_map(|r| async move {
			let room_type = services.rooms.state_accessor.get_room_type(r).await;

			if room_type.as_ref().is_err_and(|e| !e.is_not_found()) {
				return None;
			}

			let room_type_filter = RoomTypeFilter::from(room_type.ok());

			let include = if negate {
				!filter.contains(&room_type_filter)
			} else {
				filter.is_empty() || filter.contains(&room_type_filter)
			};

			include.then_some(r)
		})
		.collect()
		.await
}
