//! Bounded, session-only composer, visible-author and own-member lookups.
use crate::{Command, State, auth::Failure};
use model::{Id, Member, Patch};

pub const LIMIT: usize = 100;
pub const MAX_BYTES: usize = 128 * 1024;
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
	pub guild: Id,
	pub channel: Id,
	pub query: String,
	pub users: Vec<Id>,
	pub nonce: u64,
	pub slot: usize,
}
impl Request {
	pub fn valid(&self) -> bool {
		self.guild.0 != 0
			&& self.channel.0 != 0
			&& match self.slot {
				0 => {
					self.users.is_empty()
						&& !self.query.is_empty()
						&& self.query.len() <= 256
						&& self.query.chars().count() <= 64
						&& !self.query.chars().any(char::is_control)
				}
				1 => {
					self.query.is_empty()
						&& !self.users.is_empty()
						&& self.users.len() <= LIMIT
						&& self.users.iter().all(|id| id.0 != 0)
						&& self.users.windows(2).all(|ids| ids[0] < ids[1])
				}
				_ => false,
			}
	}
}
#[derive(Default)]
pub struct View {
	pub request: Option<Request>,
	pub rows: Vec<Member>,
	pub finished: bool,
	pub error: Option<&'static str>,
}
impl State {
	pub fn search_members(&mut self, channel: Id, query: &str, slot: usize) -> Option<Command> {
		let guild = self.channel(channel)?.guild?;
		if slot != 0
			|| !self.gateway_connected
			|| !self.can_view(channel)
			|| self.selected != Some(channel)
		{
			return None;
		}
		self.member_search_nonce = self.member_search_nonce.wrapping_add(1);
		let request = Request {
			guild,
			channel,
			query: query.to_owned(),
			users: vec![],
			nonce: self.member_search_nonce,
			slot,
		};
		if !request.valid() {
			return None;
		}
		self.member_search[slot] = View {
			request: Some(request.clone()),
			..Default::default()
		};
		Some(Command::MemberSearch(request))
	}
	pub fn request_author_members(&mut self, users: &[Id]) -> Option<Command> {
		let channel = self.selected?;
		let guild = self.channel(channel)?.guild?;
		if self.demo || !self.gateway_connected || !self.can_view(channel) {
			return None;
		}
		let mut users: Vec<_> = users
			.iter()
			.copied()
			.filter(|id| id.0 != 0)
			.take(LIMIT)
			.collect();
		users.sort_unstable();
		users.dedup();
		let view = &self.member_search[1];
		if users.is_empty()
			|| view.request.as_ref().is_some_and(|request| {
				request.channel == channel
					&& request.guild == guild
					&& (!view.finished || users.iter().all(|user| request.users.contains(user)))
			}) {
			return None;
		}
		self.member_search_nonce = self.member_search_nonce.wrapping_add(1);
		let request = Request {
			guild,
			channel,
			query: String::new(),
			users,
			nonce: self.member_search_nonce,
			slot: 1,
		};
		let rows = if view
			.request
			.as_ref()
			.is_some_and(|old| old.channel == channel && old.guild == guild)
		{
			std::mem::take(&mut self.member_search[1].rows)
		} else {
			vec![]
		};
		self.member_search[1] = View {
			rows,
			request: Some(request.clone()),
			..Default::default()
		};
		Some(Command::MemberSearch(request))
	}
	/// Looks up the signed-in member after Discord refused a channel our permissions allow. Once
	/// the answer hides that channel from us too, opens one of the server's channels we can see.
	pub fn recheck_own_member(&mut self) -> Option<Command> {
		let channel = self.own_member_recheck.take()?;
		let guild = self.channel(channel)?.guild?;
		let me = self.user.as_ref()?.id;
		if self.selected != Some(channel) || !self.gateway_connected {
			return None;
		}
		if !self.can_view(channel) {
			return self.select_guild(guild);
		}
		self.member_search_nonce = self.member_search_nonce.wrapping_add(1);
		let request = Request {
			guild,
			channel,
			query: String::new(),
			users: vec![me],
			nonce: self.member_search_nonce,
			slot: 1,
		};
		self.member_search[1] = View {
			request: Some(request.clone()),
			..Default::default()
		};
		Some(Command::MemberSearch(request))
	}
	pub(crate) fn searched_members(
		&mut self,
		request: Request,
		result: Result<Vec<Member>, Failure>,
	) {
		if !request.valid()
			|| self.selected != Some(request.channel)
			|| !self.gateway_connected
			|| !self.can_view(request.channel)
			|| self.channel(request.channel).and_then(|c| c.guild) != Some(request.guild)
			|| self.member_search[request.slot].request.as_ref() != Some(&request)
		{
			return;
		}
		self.member_search[request.slot].finished = true;
		match result {
			Ok(rows)
				if rows.len() <= LIMIT
					&& rows.iter().all(Member::valid)
					&& (request.users.is_empty()
						|| rows
							.iter()
							.all(|member| request.users.contains(&member.user.id)))
					&& rows.iter().map(Member::bytes).sum::<usize>() <= MAX_BYTES =>
			{
				// The lookup returns Discord's current member, so it also corrects our own roles
				// when a role change never reached this session.
				let own = self.user.as_ref().map(|user| user.id);
				if let Some(roles) = rows
					.iter()
					.find(|member| Some(member.user.id) == own)
					.map(|member| member.roles.clone())
				{
					let event = crate::permissions::Event::Member {
						guild: request.guild,
						roles: Patch::Value(roles),
						timeout_until: Patch::Absent,
					};
					self.server_admin.permissions_changed(&event);
					if self.update_permissions(event).is_err() {
						self.fail(Failure::Capacity);
						return;
					}
					if !self.can_view(request.channel) {
						self.own_member_recheck = Some(request.channel);
					}
				}
				if request.slot == 1 {
					for member in &rows {
						self.timeline.apply_author_membership(
							member.user.id,
							&member.roles,
							member.nick.as_deref(),
						);
					}
				}
				self.member_search[request.slot].rows = rows;
			}
			Ok(_) => {
				self.member_search[request.slot].error =
					Some("Member search exceeded its safety limit");
			}
			Err(error) => self.member_search[request.slot].error = Some(error.label()),
		}
	}
}
