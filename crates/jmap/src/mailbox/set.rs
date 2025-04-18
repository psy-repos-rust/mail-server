/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs Ltd <hello@stalw.art>
 *
 * SPDX-License-Identifier: AGPL-3.0-only OR LicenseRef-SEL
 */

use common::{auth::AccessToken, Server};
use directory::Permission;
use email::mailbox::{MailboxFnc, SCHEMA};
use jmap_proto::{
    error::set::{SetError, SetErrorType},
    method::set::{SetRequest, SetResponse},
    object::{index::ObjectIndexBuilder, mailbox::SetArguments, Object},
    response::references::EvalObjectReferences,
    types::{
        acl::Acl,
        collection::Collection,
        id::Id,
        property::Property,
        state::StateChange,
        type_state::DataType,
        value::{MaybePatchValue, SetValue, Value},
    },
};
use store::{
    query::Filter,
    roaring::RoaringBitmap,
    write::{
        assert::{AssertValue, HashedValue},
        log::ChangeLogBuilder,
        BatchBuilder, F_BITMAP, F_CLEAR, F_VALUE,
    },
};

use crate::{
    auth::acl::{AclMethods, EffectiveAcl},
    email::delete::EmailDeletion,
    JmapMethods,
};

#[allow(unused_imports)]
use email::mailbox::{UidMailbox, INBOX_ID, JUNK_ID, TRASH_ID};
use std::future::Future;

pub struct SetContext<'x> {
    account_id: u32,
    access_token: &'x AccessToken,
    is_shared: bool,
    response: SetResponse,
    mailbox_ids: RoaringBitmap,
    will_destroy: Vec<Id>,
}

pub trait MailboxSet: Sync + Send {
    fn mailbox_set(
        &self,
        request: SetRequest<SetArguments>,
        access_token: &AccessToken,
    ) -> impl Future<Output = trc::Result<SetResponse>> + Send;

    fn mailbox_destroy(
        &self,
        account_id: u32,
        document_id: u32,
        changes: &mut ChangeLogBuilder,
        access_token: &AccessToken,
        remove_emails: bool,
    ) -> impl Future<Output = trc::Result<Result<bool, SetError>>> + Send;

    fn mailbox_set_item(
        &self,
        changes_: Object<SetValue>,
        update: Option<(u32, HashedValue<Object<Value>>)>,
        ctx: &SetContext,
    ) -> impl Future<Output = trc::Result<Result<ObjectIndexBuilder, SetError>>> + Send;
}

impl MailboxSet for Server {
    #[allow(clippy::blocks_in_conditions)]
    async fn mailbox_set(
        &self,
        mut request: SetRequest<SetArguments>,
        access_token: &AccessToken,
    ) -> trc::Result<SetResponse> {
        // Prepare response
        let account_id = request.account_id.document_id();
        let on_destroy_remove_emails = request.arguments.on_destroy_remove_emails.unwrap_or(false);
        let mut ctx = SetContext {
            account_id,
            is_shared: access_token.is_shared(account_id),
            access_token,
            response: self
                .prepare_set_response(&request, Collection::Mailbox)
                .await?,
            mailbox_ids: self.mailbox_get_or_create(account_id).await?,
            will_destroy: request.unwrap_destroy(),
        };

        // Process creates
        let mut changes = ChangeLogBuilder::new();
        'create: for (id, object) in request.unwrap_create() {
            match self.mailbox_set_item(object, None, &ctx).await? {
                Ok(builder) => {
                    let mut batch = BatchBuilder::new();
                    batch
                        .with_account_id(account_id)
                        .with_collection(Collection::Mailbox);

                    if let Value::Id(parent_id) =
                        builder.changes().unwrap().get(&Property::ParentId)
                    {
                        let parent_id = parent_id.document_id();
                        if parent_id > 0 {
                            batch
                                .update_document(parent_id - 1)
                                .assert_value(Property::Value, AssertValue::Some);
                        }
                    }

                    batch.create_document().custom(builder);

                    match self
                        .core
                        .storage
                        .data
                        .write(batch.build())
                        .await
                        .and_then(|ids| ids.last_document_id())
                    {
                        Ok(document_id) => {
                            changes.log_insert(Collection::Mailbox, document_id);
                            ctx.mailbox_ids.insert(document_id);
                            ctx.response.created(id, document_id);
                        }
                        Err(err) if err.is_assertion_failure() => {
                            ctx.response.not_created.append(
                                id,
                                SetError::forbidden().with_description(
                                    "Another process deleted the parent mailbox, please try again.",
                                ),
                            );
                            continue 'create;
                        }
                        Err(err) => {
                            return Err(err.caused_by(trc::location!()));
                        }
                    }
                }
                Err(err) => {
                    ctx.response.not_created.append(id, err);
                    continue 'create;
                }
            }
        }

        // Process updates
        'update: for (id, object) in request.unwrap_update() {
            // Make sure id won't be destroyed
            if ctx.will_destroy.contains(&id) {
                ctx.response
                    .not_updated
                    .append(id, SetError::will_destroy());
                continue 'update;
            }

            // Obtain mailbox
            let document_id = id.document_id();
            if let Some(mailbox) = self
                .get_property::<HashedValue<Object<Value>>>(
                    account_id,
                    Collection::Mailbox,
                    document_id,
                    Property::Value,
                )
                .await?
            {
                // Validate ACL
                if ctx.is_shared {
                    let acl = mailbox.inner.effective_acl(access_token);
                    if !acl.contains(Acl::Modify) {
                        ctx.response.not_updated.append(
                            id,
                            SetError::forbidden()
                                .with_description("You are not allowed to modify this mailbox."),
                        );
                        continue 'update;
                    } else if object.properties.contains_key(&Property::Acl)
                        && !acl.contains(Acl::Administer)
                    {
                        ctx.response.not_updated.append(
                            id,
                            SetError::forbidden().with_description(
                                "You are not allowed to change the permissions of this mailbox.",
                            ),
                        );
                        continue 'update;
                    }
                }

                match self
                    .mailbox_set_item(object, (document_id, mailbox).into(), &ctx)
                    .await?
                {
                    Ok(builder) => {
                        let mut batch = BatchBuilder::new();
                        batch
                            .with_account_id(account_id)
                            .with_collection(Collection::Mailbox);

                        if let Value::Id(parent_id) =
                            builder.changes().unwrap().get(&Property::ParentId)
                        {
                            let parent_id = parent_id.document_id();
                            if parent_id > 0 {
                                batch
                                    .update_document(parent_id - 1)
                                    .assert_value(Property::Value, AssertValue::Some);
                            }
                        }

                        batch.update_document(document_id).custom(builder);

                        if !batch.is_empty() {
                            match self.core.storage.data.write(batch.build()).await {
                                Ok(_) => {
                                    changes.log_update(Collection::Mailbox, document_id);
                                }
                                Err(err) if err.is_assertion_failure() => {
                                    ctx.response.not_updated.append(id, SetError::forbidden().with_description(
                                        "Another process modified this mailbox, please try again.",
                                    ));
                                    continue 'update;
                                }
                                Err(err) => {
                                    return Err(err.caused_by(trc::location!()));
                                }
                            }
                        }
                        ctx.response.updated.append(id, None);
                    }
                    Err(err) => {
                        ctx.response.not_updated.append(id, err);
                        continue 'update;
                    }
                }
            } else {
                ctx.response.not_updated.append(id, SetError::not_found());
            }
        }

        // Process deletions
        let mut did_remove_emails = false;
        for id in ctx.will_destroy {
            match self
                .mailbox_destroy(
                    account_id,
                    id.document_id(),
                    &mut changes,
                    ctx.access_token,
                    on_destroy_remove_emails,
                )
                .await?
            {
                Ok(removed_emails) => {
                    did_remove_emails |= removed_emails;
                    ctx.response.destroyed.push(id);
                }
                Err(err) => {
                    ctx.response.not_destroyed.append(id, err);
                }
            }
        }

        // Write changes
        if !changes.is_empty() {
            let state_change =
                StateChange::new(account_id).with_change(DataType::Mailbox, changes.change_id);
            ctx.response.state_change = if did_remove_emails {
                state_change
                    .with_change(DataType::Email, changes.change_id)
                    .with_change(DataType::Thread, changes.change_id)
            } else {
                state_change
            }
            .into();
            ctx.response.new_state = Some(self.commit_changes(account_id, changes).await?.into());
        }

        Ok(ctx.response)
    }

    async fn mailbox_destroy(
        &self,
        account_id: u32,
        document_id: u32,
        changes: &mut ChangeLogBuilder,
        access_token: &AccessToken,
        remove_emails: bool,
    ) -> trc::Result<Result<bool, SetError>> {
        // Internal folders cannot be deleted
        #[cfg(feature = "test_mode")]
        if [INBOX_ID, TRASH_ID].contains(&document_id)
            && !access_token.has_permission(Permission::DeleteSystemFolders)
        {
            return Ok(Err(SetError::forbidden().with_description(
                "You are not allowed to delete Inbox, Junk or Trash folders.",
            )));
        }

        #[cfg(not(feature = "test_mode"))]
        if [INBOX_ID, TRASH_ID, JUNK_ID].contains(&document_id)
            && !access_token.has_permission(Permission::DeleteSystemFolders)
        {
            return Ok(Err(SetError::forbidden().with_description(
                "You are not allowed to delete Inbox, Junk or Trash folders.",
            )));
        }

        // Verify that this mailbox does not have sub-mailboxes
        if !self
            .filter(
                account_id,
                Collection::Mailbox,
                vec![Filter::eq(Property::ParentId, document_id + 1)],
            )
            .await?
            .results
            .is_empty()
        {
            return Ok(Err(SetError::new(SetErrorType::MailboxHasChild)
                .with_description("Mailbox has at least one children.")));
        }

        // Verify that the mailbox is empty
        let mut did_remove_emails = false;
        if let Some(message_ids) = self
            .get_tag(
                account_id,
                Collection::Email,
                Property::MailboxIds,
                document_id,
            )
            .await?
        {
            if remove_emails {
                // Flag removal for state change notification
                did_remove_emails = true;

                // If the message is in multiple mailboxes, untag it from the current mailbox,
                // otherwise delete it.
                let mut destroy_ids = RoaringBitmap::new();
                for (message_id, mut mailbox_ids) in self
                    .get_properties::<HashedValue<Vec<UidMailbox>>, _, _>(
                        account_id,
                        Collection::Email,
                        &message_ids,
                        Property::MailboxIds,
                    )
                    .await?
                {
                    // Remove mailbox from list
                    let orig_len = mailbox_ids.inner.len();
                    mailbox_ids.inner.retain(|id| id.mailbox_id != document_id);
                    if mailbox_ids.inner.len() == orig_len {
                        continue;
                    }

                    if !mailbox_ids.inner.is_empty() {
                        // Obtain threadId
                        if let Some(thread_id) = self
                            .get_property::<u32>(
                                account_id,
                                Collection::Email,
                                message_id,
                                Property::ThreadId,
                            )
                            .await?
                        {
                            // Untag message from mailbox
                            let mut batch = BatchBuilder::new();
                            batch
                                .with_account_id(account_id)
                                .with_collection(Collection::Email)
                                .update_document(message_id)
                                .assert_value(Property::MailboxIds, &mailbox_ids)
                                .value(Property::MailboxIds, mailbox_ids.inner, F_VALUE)
                                .value(Property::MailboxIds, document_id, F_BITMAP | F_CLEAR);
                            match self.core.storage.data.write(batch.build()).await {
                                Ok(_) => changes.log_update(
                                    Collection::Email,
                                    Id::from_parts(thread_id, message_id),
                                ),
                                Err(err) if err.is_assertion_failure() => {
                                    return Ok(Err(SetError::forbidden().with_description(
                                        concat!(
                                            "Another process modified a message in this mailbox ",
                                            "while deleting it, please try again."
                                        ),
                                    )));
                                }
                                Err(err) => {
                                    return Err(err.caused_by(trc::location!()));
                                }
                            }
                        } else {
                            trc::event!(
                                Store(trc::StoreEvent::NotFound),
                                AccountId = account_id,
                                MessageId = message_id,
                                MailboxId = document_id,
                                Details = "Message does not have a threadId.",
                                CausedBy = trc::location!(),
                            );
                        }
                    } else {
                        // Delete message
                        destroy_ids.insert(message_id);
                    }
                }

                // Bulk delete messages
                if !destroy_ids.is_empty() {
                    let (mut change, _) = self.emails_tombstone(account_id, destroy_ids).await?;
                    change.changes.remove(&(Collection::Mailbox as u8));
                    changes.merge(change);
                }
            } else {
                return Ok(Err(SetError::new(SetErrorType::MailboxHasEmail)
                    .with_description("Mailbox is not empty.")));
            }
        }

        // Obtain mailbox
        if let Some(mailbox) = self
            .get_property::<HashedValue<Object<Value>>>(
                account_id,
                Collection::Mailbox,
                document_id,
                Property::Value,
            )
            .await?
        {
            // Validate ACLs
            if access_token.is_shared(account_id) {
                let acl = mailbox.inner.effective_acl(access_token);
                if !acl.contains(Acl::Administer) {
                    if !acl.contains(Acl::Delete) {
                        return Ok(Err(SetError::forbidden()
                            .with_description("You are not allowed to delete this mailbox.")));
                    } else if remove_emails && !acl.contains(Acl::RemoveItems) {
                        return Ok(Err(SetError::forbidden().with_description(
                            "You are not allowed to delete emails from this mailbox.",
                        )));
                    }
                }
            }

            let mut batch = BatchBuilder::new();
            batch
                .with_account_id(account_id)
                .with_collection(Collection::Mailbox)
                .delete_document(document_id)
                .value(Property::EmailIds, (), F_VALUE | F_CLEAR)
                .custom(ObjectIndexBuilder::new(SCHEMA).with_current(mailbox));

            match self.core.storage.data.write(batch.build()).await {
                Ok(_) => {
                    changes.log_delete(Collection::Mailbox, document_id);
                    Ok(Ok(did_remove_emails))
                }
                Err(err) if err.is_assertion_failure() => Ok(Err(SetError::forbidden()
                    .with_description(concat!(
                        "Another process modified this mailbox ",
                        "while deleting it, please try again."
                    )))),
                Err(err) => Err(err.caused_by(trc::location!())),
            }
        } else {
            Ok(Err(SetError::not_found()))
        }
    }

    #[allow(clippy::blocks_in_conditions)]
    async fn mailbox_set_item(
        &self,
        changes_: Object<SetValue>,
        update: Option<(u32, HashedValue<Object<Value>>)>,
        ctx: &SetContext<'_>,
    ) -> trc::Result<Result<ObjectIndexBuilder, SetError>> {
        // Parse properties
        let mut changes = Object::with_capacity(changes_.properties.len());
        for (property, value) in changes_.properties {
            let value = match ctx.response.eval_object_references(value) {
                Ok(value) => value,
                Err(err) => {
                    return Ok(Err(err));
                }
            };
            let value = match (&property, value) {
                (Property::Name, MaybePatchValue::Value(Value::Text(value))) => {
                    let value = value.trim();
                    if !value.is_empty() && value.len() < self.core.jmap.mailbox_name_max_len {
                        Value::Text(value.to_string())
                    } else {
                        return Ok(Err(SetError::invalid_properties()
                            .with_property(Property::Name)
                            .with_description(
                                if !value.is_empty() {
                                    "Mailbox name is too long."
                                } else {
                                    "Mailbox name cannot be empty."
                                }
                                .to_string(),
                            )));
                    }
                }
                (Property::ParentId, MaybePatchValue::Value(Value::Id(value))) => {
                    let parent_id = value.document_id();
                    if ctx.will_destroy.contains(&value) {
                        return Ok(Err(SetError::will_destroy()
                            .with_description("Parent ID will be destroyed.")));
                    } else if !ctx.mailbox_ids.contains(parent_id) {
                        return Ok(Err(SetError::invalid_properties()
                            .with_description("Parent ID does not exist.")));
                    }

                    Value::Id((parent_id + 1).into())
                }
                (Property::ParentId, MaybePatchValue::Value(Value::Null)) => Value::Id(0u64.into()),
                (Property::IsSubscribed, MaybePatchValue::Value(Value::Bool(subscribe))) => {
                    if let Some((_, current_fields)) = update.as_ref() {
                        if let Some(value) = current_fields
                            .inner
                            .mailbox_subscribe(ctx.access_token.primary_id(), subscribe)
                        {
                            value
                        } else {
                            continue;
                        }
                    } else if subscribe {
                        Value::List(vec![Value::Id(ctx.access_token.primary_id().into())])
                    } else {
                        continue;
                    }
                }
                (Property::Role, MaybePatchValue::Value(Value::Text(value))) => {
                    let role = value.trim().to_lowercase();
                    if [
                        "inbox", "trash", "spam", "junk", "drafts", "archive", "sent",
                    ]
                    .contains(&role.as_str())
                    {
                        Value::Text(role)
                    } else {
                        return Ok(Err(SetError::invalid_properties()
                            .with_property(Property::Role)
                            .with_description(format!("Invalid role {role:?}."))));
                    }
                }
                (Property::Role, MaybePatchValue::Value(Value::Null)) => Value::Null,
                (Property::SortOrder, MaybePatchValue::Value(Value::UnsignedInt(value))) => {
                    Value::UnsignedInt(value)
                }
                (Property::Acl, value) => {
                    match self
                        .acl_set(&mut changes, update.as_ref().map(|(_, obj)| obj), value)
                        .await
                    {
                        Ok(_) => continue,
                        Err(err) => {
                            return Ok(Err(err));
                        }
                    }
                }

                _ => {
                    return Ok(Err(SetError::invalid_properties()
                        .with_property(property)
                        .with_description("Invalid property or value.".to_string())))
                }
            };

            changes.append(property, value);
        }

        // Validate depth and circular parent-child relationship
        if let Value::Id(mailbox_parent_id) = changes.get(&Property::ParentId) {
            let current_mailbox_id = update
                .as_ref()
                .map_or(u32::MAX, |(mailbox_id, _)| *mailbox_id + 1);
            let mut mailbox_parent_id = mailbox_parent_id.document_id();
            let mut success = false;
            for depth in 0..self.core.jmap.mailbox_max_depth {
                if mailbox_parent_id == current_mailbox_id {
                    return Ok(Err(SetError::invalid_properties()
                        .with_property(Property::ParentId)
                        .with_description("Mailbox cannot be a parent of itself.")));
                } else if mailbox_parent_id == 0 {
                    if depth == 0 && ctx.is_shared {
                        return Ok(Err(SetError::forbidden()
                            .with_description("You are not allowed to create root folders.")));
                    }
                    success = true;
                    break;
                }
                let parent_document_id = mailbox_parent_id - 1;

                if let Some(mut fields) = self
                    .get_property::<Object<Value>>(
                        ctx.account_id,
                        Collection::Mailbox,
                        parent_document_id,
                        Property::Value,
                    )
                    .await?
                {
                    if depth == 0
                        && ctx.is_shared
                        && !fields
                            .effective_acl(ctx.access_token)
                            .contains_any([Acl::CreateChild, Acl::Administer].into_iter())
                    {
                        return Ok(Err(SetError::forbidden().with_description(
                            "You are not allowed to create sub mailboxes under this mailbox.",
                        )));
                    }

                    mailbox_parent_id = fields
                        .properties
                        .remove(&Property::ParentId)
                        .and_then(|v| v.try_unwrap_id().map(|id| id.document_id()))
                        .unwrap_or(0);
                } else if ctx.mailbox_ids.contains(parent_document_id) {
                    // Parent mailbox is probably created within the same request
                    success = true;
                    break;
                } else {
                    return Ok(Err(SetError::invalid_properties()
                        .with_property(Property::ParentId)
                        .with_description("Mailbox parent does not exist.")));
                }
            }

            if !success {
                return Ok(Err(SetError::invalid_properties()
                    .with_property(Property::ParentId)
                    .with_description(
                        "Mailbox parent-child relationship is too deep.",
                    )));
            }
        } else if update.is_none() {
            // Set parentId if the field is missing
            changes.append(Property::ParentId, Value::Id(0u64.into()));
        }

        // Generate IMAP UID validity
        if update.is_none() {
            changes.append(
                Property::Cid,
                Value::UnsignedInt(rand::random::<u32>() as u64),
            );
        }

        // Verify that the mailbox role is unique.
        if let Value::Text(mailbox_role) = changes.get(&Property::Role) {
            if update
                .as_ref()
                .map(|(_, update)| update.inner.get(&Property::Role))
                .and_then(|v| v.as_string())
                .unwrap_or_default()
                != mailbox_role
            {
                if !self
                    .filter(
                        ctx.account_id,
                        Collection::Mailbox,
                        vec![Filter::eq(Property::Role, mailbox_role.as_str())],
                    )
                    .await?
                    .results
                    .is_empty()
                {
                    return Ok(Err(SetError::invalid_properties()
                        .with_property(Property::Role)
                        .with_description(format!(
                            "A mailbox with role '{}' already exists.",
                            mailbox_role
                        ))));
                }

                // Role of internal folders cannot be modified
                if update.as_ref().is_some_and(|(document_id, _)| {
                    *document_id == INBOX_ID || *document_id == TRASH_ID
                }) {
                    return Ok(Err(SetError::invalid_properties()
                        .with_property(Property::Role)
                        .with_description(
                            "You are not allowed to change the role of Inbox or Trash folders.",
                        )));
                }
            }
        }

        // Verify that the mailbox name is unique.
        if let Value::Text(mailbox_name) = changes.get(&Property::Name) {
            // Obtain parent mailbox id
            if let Some(parent_mailbox_id) = if let Some(mailbox_parent_id) = &changes
                .properties
                .get(&Property::ParentId)
                .and_then(|id| id.as_id().map(|id| id.document_id()))
            {
                (*mailbox_parent_id).into()
            } else if let Some((_, current_fields)) = &update {
                if current_fields
                    .inner
                    .properties
                    .get(&Property::Name)
                    .and_then(|n| n.as_string())
                    != Some(mailbox_name)
                {
                    current_fields
                        .inner
                        .properties
                        .get(&Property::ParentId)
                        .and_then(|id| id.as_id().map(|id| id.document_id()))
                        .unwrap_or_default()
                        .into()
                } else {
                    None
                }
            } else {
                0.into()
            } {
                if !self
                    .filter(
                        ctx.account_id,
                        Collection::Mailbox,
                        vec![
                            Filter::eq(Property::Name, mailbox_name.as_str()),
                            Filter::eq(Property::ParentId, parent_mailbox_id),
                        ],
                    )
                    .await?
                    .results
                    .is_empty()
                {
                    return Ok(Err(SetError::invalid_properties()
                        .with_property(Property::Name)
                        .with_description(format!(
                            "A mailbox with name '{}' already exists.",
                            mailbox_name
                        ))));
                }
            }
        }

        // Refresh ACLs
        let current = update.map(|(_, current)| current);
        if changes.properties.contains_key(&Property::Acl) {
            self.refresh_acls(&changes, &current).await;
        }

        // Validate
        Ok(ObjectIndexBuilder::new(SCHEMA)
            .with_changes(changes)
            .with_current_opt(current)
            .validate())
    }
}

pub trait MailboxSubscribe {
    fn mailbox_subscribe(&self, account_id: u32, subscribed: bool) -> Option<Value>;
}

impl MailboxSubscribe for Object<Value> {
    fn mailbox_subscribe(&self, account_id: u32, subscribe: bool) -> Option<Value> {
        let account_id = Value::Id(account_id.into());
        if let Value::List(subscriptions) = self.get(&Property::IsSubscribed) {
            if subscribe {
                if !subscriptions.contains(&account_id) {
                    let mut current_subscriptions = subscriptions.clone();
                    current_subscriptions.push(account_id);
                    Value::List(current_subscriptions).into()
                } else {
                    None
                }
            } else if subscriptions.contains(&account_id) {
                if subscriptions.len() > 1 {
                    Value::List(
                        subscriptions
                            .iter()
                            .filter(|id| *id != &account_id)
                            .cloned()
                            .collect(),
                    )
                    .into()
                } else {
                    Value::Null.into()
                }
            } else {
                None
            }
        } else if subscribe {
            Value::List(vec![account_id]).into()
        } else {
            None
        }
    }
}
