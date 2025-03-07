use crate::{
    binding_model::{BindGroup, BindGroupLayout, PipelineLayout},
    command::RenderBundle,
    device::{
        queue::{EncoderInFlight, SubmittedWorkDoneClosure, TempResource},
        DeviceError, DeviceLostClosure,
    },
    hal_api::HalApi,
    id,
    lock::Mutex,
    pipeline::{ComputePipeline, RenderPipeline},
    resource::{
        self, Buffer, DestroyedBuffer, DestroyedTexture, QuerySet, Resource, Sampler,
        StagingBuffer, Texture, TextureView,
    },
    snatch::SnatchGuard,
    track::{ResourceTracker, Tracker, TrackerIndex},
    FastHashMap, SubmissionIndex,
};
use smallvec::SmallVec;

use std::sync::Arc;
use thiserror::Error;

/// A struct that keeps lists of resources that are no longer needed by the user.
pub(crate) struct ResourceMaps<A: HalApi> {
    pub buffers: FastHashMap<TrackerIndex, Arc<Buffer<A>>>,
    pub staging_buffers: FastHashMap<TrackerIndex, Arc<StagingBuffer<A>>>,
    pub textures: FastHashMap<TrackerIndex, Arc<Texture<A>>>,
    pub texture_views: FastHashMap<TrackerIndex, Arc<TextureView<A>>>,
    pub samplers: FastHashMap<TrackerIndex, Arc<Sampler<A>>>,
    pub bind_groups: FastHashMap<TrackerIndex, Arc<BindGroup<A>>>,
    pub bind_group_layouts: FastHashMap<TrackerIndex, Arc<BindGroupLayout<A>>>,
    pub render_pipelines: FastHashMap<TrackerIndex, Arc<RenderPipeline<A>>>,
    pub compute_pipelines: FastHashMap<TrackerIndex, Arc<ComputePipeline<A>>>,
    pub pipeline_layouts: FastHashMap<TrackerIndex, Arc<PipelineLayout<A>>>,
    pub render_bundles: FastHashMap<TrackerIndex, Arc<RenderBundle<A>>>,
    pub query_sets: FastHashMap<TrackerIndex, Arc<QuerySet<A>>>,
    pub destroyed_buffers: FastHashMap<TrackerIndex, Arc<DestroyedBuffer<A>>>,
    pub destroyed_textures: FastHashMap<TrackerIndex, Arc<DestroyedTexture<A>>>,
}

impl<A: HalApi> ResourceMaps<A> {
    pub(crate) fn new() -> Self {
        ResourceMaps {
            buffers: FastHashMap::default(),
            staging_buffers: FastHashMap::default(),
            textures: FastHashMap::default(),
            texture_views: FastHashMap::default(),
            samplers: FastHashMap::default(),
            bind_groups: FastHashMap::default(),
            bind_group_layouts: FastHashMap::default(),
            render_pipelines: FastHashMap::default(),
            compute_pipelines: FastHashMap::default(),
            pipeline_layouts: FastHashMap::default(),
            render_bundles: FastHashMap::default(),
            query_sets: FastHashMap::default(),
            destroyed_buffers: FastHashMap::default(),
            destroyed_textures: FastHashMap::default(),
        }
    }

    pub(crate) fn clear(&mut self) {
        let ResourceMaps {
            buffers,
            staging_buffers,
            textures,
            texture_views,
            samplers,
            bind_groups,
            bind_group_layouts,
            render_pipelines,
            compute_pipelines,
            pipeline_layouts,
            render_bundles,
            query_sets,
            destroyed_buffers,
            destroyed_textures,
        } = self;
        buffers.clear();
        staging_buffers.clear();
        textures.clear();
        texture_views.clear();
        samplers.clear();
        bind_groups.clear();
        bind_group_layouts.clear();
        render_pipelines.clear();
        compute_pipelines.clear();
        pipeline_layouts.clear();
        render_bundles.clear();
        query_sets.clear();
        destroyed_buffers.clear();
        destroyed_textures.clear();
    }

    pub(crate) fn extend(&mut self, mut other: Self) {
        let ResourceMaps {
            buffers,
            staging_buffers,
            textures,
            texture_views,
            samplers,
            bind_groups,
            bind_group_layouts,
            render_pipelines,
            compute_pipelines,
            pipeline_layouts,
            render_bundles,
            query_sets,
            destroyed_buffers,
            destroyed_textures,
        } = self;
        buffers.extend(other.buffers.drain());
        staging_buffers.extend(other.staging_buffers.drain());
        textures.extend(other.textures.drain());
        texture_views.extend(other.texture_views.drain());
        samplers.extend(other.samplers.drain());
        bind_groups.extend(other.bind_groups.drain());
        bind_group_layouts.extend(other.bind_group_layouts.drain());
        render_pipelines.extend(other.render_pipelines.drain());
        compute_pipelines.extend(other.compute_pipelines.drain());
        pipeline_layouts.extend(other.pipeline_layouts.drain());
        render_bundles.extend(other.render_bundles.drain());
        query_sets.extend(other.query_sets.drain());
        destroyed_buffers.extend(other.destroyed_buffers.drain());
        destroyed_textures.extend(other.destroyed_textures.drain());
    }
}

/// A command submitted to the GPU for execution.
///
/// ## Keeping resources alive while the GPU is using them
///
/// [`wgpu_hal`] requires that, when a command is submitted to a queue, all the
/// resources it uses must remain alive until it has finished executing.
///
/// The natural way to satisfy this would be for `ActiveSubmission` to hold
/// strong references to all the resources used by its commands. However, that
/// would entail dropping those strong references every time a queue submission
/// finishes, adjusting the reference counts of all the resources it used. This
/// is usually needless work: it's rare for the active submission queue to be
/// the final reference to an object. Usually the user is still holding on to
/// it.
///
/// To avoid this, an `ActiveSubmission` does not initially hold any strong
/// references to its commands' resources. Instead, each resource tracks the
/// most recent submission index at which it has been used in
/// [`ResourceInfo::submission_index`]. When the user drops a resource, if the
/// submission in which it was last used is still present in the device's queue,
/// we add the resource to [`ActiveSubmission::last_resources`]. Finally, when
/// this `ActiveSubmission` is dequeued and dropped in
/// [`LifetimeTracker::triage_submissions`], we drop `last_resources` along with
/// it. Thus, unless a resource is dropped by the user, it doesn't need to be
/// touched at all when processing completed work.
///
/// However, it's not clear that this is effective. See [#5560].
///
/// [`wgpu_hal`]: hal
/// [`ResourceInfo::submission_index`]: crate::resource::ResourceInfo
/// [#5560]: https://github.com/gfx-rs/wgpu/issues/5560
struct ActiveSubmission<A: HalApi> {
    /// The index of the submission we track.
    ///
    /// When `Device::fence`'s value is greater than or equal to this, our queue
    /// submission has completed.
    index: SubmissionIndex,

    /// Resources to be freed once this queue submission has completed.
    ///
    /// When the device is polled, for completed submissions,
    /// `triage_submissions` removes resources that don't need to be held alive any longer
    /// from there.
    ///
    /// This includes things like temporary resources and resources that are
    /// used by submitted commands but have been dropped by the user (meaning that
    /// this submission is their last reference.)
    last_resources: ResourceMaps<A>,

    /// Buffers to be mapped once this submission has completed.
    mapped: Vec<Arc<Buffer<A>>>,

    /// Command buffers used by this submission, and the encoder that owns them.
    ///
    /// [`wgpu_hal::Queue::submit`] requires the submitted command buffers to
    /// remain alive until the submission has completed execution. Command
    /// encoders double as allocation pools for command buffers, so holding them
    /// here and cleaning them up in [`LifetimeTracker::triage_submissions`]
    /// satisfies that requirement.
    ///
    /// Once this submission has completed, the command buffers are reset and
    /// the command encoder is recycled.
    ///
    /// [`wgpu_hal::Queue::submit`]: hal::Queue::submit
    encoders: Vec<EncoderInFlight<A>>,

    /// List of queue "on_submitted_work_done" closures to be called once this
    /// submission has completed.
    work_done_closures: SmallVec<[SubmittedWorkDoneClosure; 1]>,
}

#[derive(Clone, Debug, Error)]
#[non_exhaustive]
pub enum WaitIdleError {
    #[error(transparent)]
    Device(#[from] DeviceError),
    #[error("Tried to wait using a submission index from the wrong device. Submission index is from device {0:?}. Called poll on device {1:?}.")]
    WrongSubmissionIndex(id::QueueId, id::DeviceId),
    #[error("GPU got stuck :(")]
    StuckGpu,
}

/// Resource tracking for a device.
///
/// ## Host mapping buffers
///
/// A buffer cannot be mapped until all active queue submissions that use it
/// have completed. To that end:
///
/// -   Each buffer's `ResourceInfo::submission_index` records the index of the
///     most recent queue submission that uses that buffer.
///
/// -   Calling `Global::buffer_map_async` adds the buffer to
///     `self.mapped`, and changes `Buffer::map_state` to prevent it
///     from being used in any new submissions.
///
/// -   When the device is polled, the following `LifetimeTracker` methods decide
///     what should happen next:
///
///     1)  `triage_mapped` drains `self.mapped`, checking the submission index
///         of each buffer against the queue submissions that have finished
///         execution. Buffers used by submissions still in flight go in
///         `self.active[index].mapped`, and the rest go into
///         `self.ready_to_map`.
///
///     2)  `triage_submissions` moves entries in `self.active[i]` for completed
///         submissions to `self.ready_to_map`.  At this point, both
///         `self.active` and `self.ready_to_map` are up to date with the given
///         submission index.
///
///     3)  `handle_mapping` drains `self.ready_to_map` and actually maps the
///         buffers, collecting a list of notification closures to call. But any
///         buffers that were dropped by the user get moved to
///         `self.free_resources`.
///
/// Only calling `Global::buffer_map_async` clones a new `Arc` for the
/// buffer. This new `Arc` is only dropped by `handle_mapping`.
pub(crate) struct LifetimeTracker<A: HalApi> {
    /// Resources that the user has requested be mapped, but which are used by
    /// queue submissions still in flight.
    mapped: Vec<Arc<Buffer<A>>>,

    /// Buffers can be used in a submission that is yet to be made, by the
    /// means of `write_buffer()`, so we have a special place for them.
    pub future_suspected_buffers: Vec<Arc<Buffer<A>>>,

    /// Textures can be used in the upcoming submission by `write_texture`.
    pub future_suspected_textures: Vec<Arc<Texture<A>>>,

    /// Resources whose user handle has died (i.e. drop/destroy has been called)
    /// and will likely be ready for destruction soon.
    pub suspected_resources: ResourceMaps<A>,

    /// Resources used by queue submissions still in flight. One entry per
    /// submission, with older submissions appearing before younger.
    ///
    /// Entries are added by `track_submission` and drained by
    /// `LifetimeTracker::triage_submissions`. Lots of methods contribute data
    /// to particular entries.
    active: Vec<ActiveSubmission<A>>,

    /// Buffers the user has asked us to map, and which are not used by any
    /// queue submission still in flight.
    ready_to_map: Vec<Arc<Buffer<A>>>,

    /// Queue "on_submitted_work_done" closures that were initiated for while there is no
    /// currently pending submissions. These cannot be immediately invoked as they
    /// must happen _after_ all mapped buffer callbacks are mapped, so we defer them
    /// here until the next time the device is maintained.
    work_done_closures: SmallVec<[SubmittedWorkDoneClosure; 1]>,

    /// Closure to be called on "lose the device". This is invoked directly by
    /// device.lose or by the UserCallbacks returned from maintain when the device
    /// has been destroyed and its queues are empty.
    pub device_lost_closure: Option<DeviceLostClosure>,
}

impl<A: HalApi> LifetimeTracker<A> {
    pub fn new() -> Self {
        Self {
            mapped: Vec::new(),
            future_suspected_buffers: Vec::new(),
            future_suspected_textures: Vec::new(),
            suspected_resources: ResourceMaps::new(),
            active: Vec::new(),
            ready_to_map: Vec::new(),
            work_done_closures: SmallVec::new(),
            device_lost_closure: None,
        }
    }

    /// Return true if there are no queue submissions still in flight.
    pub fn queue_empty(&self) -> bool {
        self.active.is_empty()
    }

    /// Start tracking resources associated with a new queue submission.
    pub fn track_submission(
        &mut self,
        index: SubmissionIndex,
        temp_resources: impl Iterator<Item = TempResource<A>>,
        encoders: Vec<EncoderInFlight<A>>,
    ) {
        let mut last_resources = ResourceMaps::new();
        for res in temp_resources {
            match res {
                TempResource::Buffer(raw) => {
                    last_resources
                        .buffers
                        .insert(raw.as_info().tracker_index(), raw);
                }
                TempResource::StagingBuffer(raw) => {
                    last_resources
                        .staging_buffers
                        .insert(raw.as_info().tracker_index(), raw);
                }
                TempResource::DestroyedBuffer(destroyed) => {
                    last_resources
                        .destroyed_buffers
                        .insert(destroyed.tracker_index, destroyed);
                }
                TempResource::Texture(raw) => {
                    last_resources
                        .textures
                        .insert(raw.as_info().tracker_index(), raw);
                }
                TempResource::DestroyedTexture(destroyed) => {
                    last_resources
                        .destroyed_textures
                        .insert(destroyed.tracker_index, destroyed);
                }
            }
        }

        self.active.push(ActiveSubmission {
            index,
            last_resources,
            mapped: Vec::new(),
            encoders,
            work_done_closures: SmallVec::new(),
        });
    }

    pub fn post_submit(&mut self) {
        for v in self.future_suspected_buffers.drain(..) {
            self.suspected_resources
                .buffers
                .insert(v.as_info().tracker_index(), v);
        }
        for v in self.future_suspected_textures.drain(..) {
            self.suspected_resources
                .textures
                .insert(v.as_info().tracker_index(), v);
        }
    }

    pub(crate) fn map(&mut self, value: &Arc<Buffer<A>>) {
        self.mapped.push(value.clone());
    }

    /// Sort out the consequences of completed submissions.
    ///
    /// Assume that all submissions up through `last_done` have completed.
    ///
    /// -   Buffers used by those submissions are now ready to map, if requested.
    ///     Add any buffers in the submission's [`mapped`] list to
    ///     [`self.ready_to_map`], where [`LifetimeTracker::handle_mapping`]
    ///     will find them.
    ///
    /// -   Resources whose final use was in those submissions are now ready to
    ///     free. Dropping the submission's [`last_resources`] table does so.
    ///
    /// Return a list of [`SubmittedWorkDoneClosure`]s to run.
    ///
    /// [`mapped`]: ActiveSubmission::mapped
    /// [`self.ready_to_map`]: LifetimeTracker::ready_to_map
    /// [`last_resources`]: ActiveSubmission::last_resources
    /// [`SubmittedWorkDoneClosure`]: crate::device::queue::SubmittedWorkDoneClosure
    #[must_use]
    pub fn triage_submissions(
        &mut self,
        last_done: SubmissionIndex,
        command_allocator: &crate::command::CommandAllocator<A>,
    ) -> SmallVec<[SubmittedWorkDoneClosure; 1]> {
        profiling::scope!("triage_submissions");

        //TODO: enable when `is_sorted_by_key` is stable
        //debug_assert!(self.active.is_sorted_by_key(|a| a.index));
        let done_count = self
            .active
            .iter()
            .position(|a| a.index > last_done)
            .unwrap_or(self.active.len());

        let mut work_done_closures: SmallVec<_> = self.work_done_closures.drain(..).collect();
        for a in self.active.drain(..done_count) {
            log::debug!("Active submission {} is done", a.index);
            self.ready_to_map.extend(a.mapped);
            for encoder in a.encoders {
                let raw = unsafe { encoder.land() };
                command_allocator.release_encoder(raw);
            }
            work_done_closures.extend(a.work_done_closures);
        }
        work_done_closures
    }

    pub fn schedule_resource_destruction(
        &mut self,
        temp_resource: TempResource<A>,
        last_submit_index: SubmissionIndex,
    ) {
        let resources = self
            .active
            .iter_mut()
            .find(|a| a.index == last_submit_index)
            .map(|a| &mut a.last_resources);
        if let Some(resources) = resources {
            match temp_resource {
                TempResource::Buffer(raw) => {
                    resources.buffers.insert(raw.as_info().tracker_index(), raw);
                }
                TempResource::StagingBuffer(raw) => {
                    resources
                        .staging_buffers
                        .insert(raw.as_info().tracker_index(), raw);
                }
                TempResource::DestroyedBuffer(destroyed) => {
                    resources
                        .destroyed_buffers
                        .insert(destroyed.tracker_index, destroyed);
                }
                TempResource::Texture(raw) => {
                    resources
                        .textures
                        .insert(raw.as_info().tracker_index(), raw);
                }
                TempResource::DestroyedTexture(destroyed) => {
                    resources
                        .destroyed_textures
                        .insert(destroyed.tracker_index, destroyed);
                }
            }
        }
    }

    pub fn add_work_done_closure(&mut self, closure: SubmittedWorkDoneClosure) {
        match self.active.last_mut() {
            Some(active) => {
                active.work_done_closures.push(closure);
            }
            // We must defer the closure until all previously occurring map_async closures
            // have fired. This is required by the spec.
            None => {
                self.work_done_closures.push(closure);
            }
        }
    }
}

impl<A: HalApi> LifetimeTracker<A> {
    fn triage_resources<R>(
        resources_map: &mut FastHashMap<TrackerIndex, Arc<R>>,
        active: &mut [ActiveSubmission<A>],
        trackers: &mut impl ResourceTracker,
        get_resource_map: impl Fn(&mut ResourceMaps<A>) -> &mut FastHashMap<TrackerIndex, Arc<R>>,
    ) -> Vec<Arc<R>>
    where
        R: Resource,
    {
        let mut removed_resources = Vec::new();
        resources_map.retain(|&index, resource| {
            let submit_index = resource.as_info().submission_index();
            let non_referenced_resources = active
                .iter_mut()
                .find(|a| a.index == submit_index)
                .map(|a| &mut a.last_resources);

            let is_removed = trackers.remove_abandoned(index);
            if is_removed {
                removed_resources.push(resource.clone());
                if let Some(resources) = non_referenced_resources {
                    get_resource_map(resources).insert(index, resource.clone());
                }
            }
            !is_removed
        });
        removed_resources
    }

    fn triage_suspected_render_bundles(&mut self, trackers: &Mutex<Tracker<A>>) -> &mut Self {
        let mut trackers = trackers.lock();
        let resource_map = &mut self.suspected_resources.render_bundles;
        let mut removed_resources = Self::triage_resources(
            resource_map,
            self.active.as_mut_slice(),
            &mut trackers.bundles,
            |maps| &mut maps.render_bundles,
        );
        removed_resources.drain(..).for_each(|bundle| {
            for v in bundle.used.buffers.write().drain_resources() {
                self.suspected_resources
                    .buffers
                    .insert(v.as_info().tracker_index(), v);
            }
            for v in bundle.used.textures.write().drain_resources() {
                self.suspected_resources
                    .textures
                    .insert(v.as_info().tracker_index(), v);
            }
            for v in bundle.used.bind_groups.write().drain_resources() {
                self.suspected_resources
                    .bind_groups
                    .insert(v.as_info().tracker_index(), v);
            }
            for v in bundle.used.render_pipelines.write().drain_resources() {
                self.suspected_resources
                    .render_pipelines
                    .insert(v.as_info().tracker_index(), v);
            }
            for v in bundle.used.query_sets.write().drain_resources() {
                self.suspected_resources
                    .query_sets
                    .insert(v.as_info().tracker_index(), v);
            }
        });
        self
    }

    fn triage_suspected_bind_groups(&mut self, trackers: &Mutex<Tracker<A>>) -> &mut Self {
        let mut trackers = trackers.lock();
        let resource_map = &mut self.suspected_resources.bind_groups;
        let mut removed_resource = Self::triage_resources(
            resource_map,
            self.active.as_mut_slice(),
            &mut trackers.bind_groups,
            |maps| &mut maps.bind_groups,
        );
        removed_resource.drain(..).for_each(|bind_group| {
            for v in bind_group.used.buffers.drain_resources() {
                self.suspected_resources
                    .buffers
                    .insert(v.as_info().tracker_index(), v);
            }
            for v in bind_group.used.textures.drain_resources() {
                self.suspected_resources
                    .textures
                    .insert(v.as_info().tracker_index(), v);
            }
            for v in bind_group.used.views.drain_resources() {
                self.suspected_resources
                    .texture_views
                    .insert(v.as_info().tracker_index(), v);
            }
            for v in bind_group.used.samplers.drain_resources() {
                self.suspected_resources
                    .samplers
                    .insert(v.as_info().tracker_index(), v);
            }

            self.suspected_resources.bind_group_layouts.insert(
                bind_group.layout.as_info().tracker_index(),
                bind_group.layout.clone(),
            );
        });
        self
    }

    fn triage_suspected_texture_views(&mut self, trackers: &Mutex<Tracker<A>>) -> &mut Self {
        let mut trackers = trackers.lock();
        let resource_map = &mut self.suspected_resources.texture_views;
        Self::triage_resources(
            resource_map,
            self.active.as_mut_slice(),
            &mut trackers.views,
            |maps| &mut maps.texture_views,
        );
        self
    }

    fn triage_suspected_textures(&mut self, trackers: &Mutex<Tracker<A>>) -> &mut Self {
        let mut trackers = trackers.lock();
        let resource_map = &mut self.suspected_resources.textures;
        Self::triage_resources(
            resource_map,
            self.active.as_mut_slice(),
            &mut trackers.textures,
            |maps| &mut maps.textures,
        );

        // We may have been suspected because a texture view or bind group
        // referring to us was dropped. Remove stale weak references, so that
        // the backlink table doesn't grow without bound.
        for texture in self.suspected_resources.textures.values() {
            texture.views.lock().retain(|view| view.strong_count() > 0);
            texture
                .bind_groups
                .lock()
                .retain(|bg| bg.strong_count() > 0);
        }

        self
    }

    fn triage_suspected_samplers(&mut self, trackers: &Mutex<Tracker<A>>) -> &mut Self {
        let mut trackers = trackers.lock();
        let resource_map = &mut self.suspected_resources.samplers;
        Self::triage_resources(
            resource_map,
            self.active.as_mut_slice(),
            &mut trackers.samplers,
            |maps| &mut maps.samplers,
        );
        self
    }

    fn triage_suspected_buffers(&mut self, trackers: &Mutex<Tracker<A>>) -> &mut Self {
        let mut trackers = trackers.lock();
        let resource_map = &mut self.suspected_resources.buffers;
        Self::triage_resources(
            resource_map,
            self.active.as_mut_slice(),
            &mut trackers.buffers,
            |maps| &mut maps.buffers,
        );

        // We may have been suspected because a bind group referring to us was
        // dropped. Remove stale weak references, so that the backlink table
        // doesn't grow without bound.
        for buffer in self.suspected_resources.buffers.values() {
            buffer.bind_groups.lock().retain(|bg| bg.strong_count() > 0);
        }

        self
    }

    fn triage_suspected_destroyed_buffers(&mut self) {
        for (id, buffer) in self.suspected_resources.destroyed_buffers.drain() {
            let submit_index = buffer.submission_index;
            if let Some(resources) = self.active.iter_mut().find(|a| a.index == submit_index) {
                resources
                    .last_resources
                    .destroyed_buffers
                    .insert(id, buffer);
            }
        }
    }

    fn triage_suspected_destroyed_textures(&mut self) {
        for (id, texture) in self.suspected_resources.destroyed_textures.drain() {
            let submit_index = texture.submission_index;
            if let Some(resources) = self.active.iter_mut().find(|a| a.index == submit_index) {
                resources
                    .last_resources
                    .destroyed_textures
                    .insert(id, texture);
            }
        }
    }

    fn triage_suspected_compute_pipelines(&mut self, trackers: &Mutex<Tracker<A>>) -> &mut Self {
        let mut trackers = trackers.lock();
        let resource_map = &mut self.suspected_resources.compute_pipelines;
        let mut removed_resources = Self::triage_resources(
            resource_map,
            self.active.as_mut_slice(),
            &mut trackers.compute_pipelines,
            |maps| &mut maps.compute_pipelines,
        );
        removed_resources.drain(..).for_each(|compute_pipeline| {
            self.suspected_resources.pipeline_layouts.insert(
                compute_pipeline.layout.as_info().tracker_index(),
                compute_pipeline.layout.clone(),
            );
        });
        self
    }

    fn triage_suspected_render_pipelines(&mut self, trackers: &Mutex<Tracker<A>>) -> &mut Self {
        let mut trackers = trackers.lock();
        let resource_map = &mut self.suspected_resources.render_pipelines;
        let mut removed_resources = Self::triage_resources(
            resource_map,
            self.active.as_mut_slice(),
            &mut trackers.render_pipelines,
            |maps| &mut maps.render_pipelines,
        );
        removed_resources.drain(..).for_each(|render_pipeline| {
            self.suspected_resources.pipeline_layouts.insert(
                render_pipeline.layout.as_info().tracker_index(),
                render_pipeline.layout.clone(),
            );
        });
        self
    }

    fn triage_suspected_pipeline_layouts(&mut self) -> &mut Self {
        let mut removed_resources = Vec::new();
        self.suspected_resources
            .pipeline_layouts
            .retain(|_pipeline_layout_id, pipeline_layout| {
                removed_resources.push(pipeline_layout.clone());
                false
            });
        removed_resources.drain(..).for_each(|pipeline_layout| {
            for bgl in &pipeline_layout.bind_group_layouts {
                self.suspected_resources
                    .bind_group_layouts
                    .insert(bgl.as_info().tracker_index(), bgl.clone());
            }
        });
        self
    }

    fn triage_suspected_bind_group_layouts(&mut self) -> &mut Self {
        //Note: this has to happen after all the suspected pipelines are destroyed
        //Note: nothing else can bump the refcount since the guard is locked exclusively
        //Note: same BGL can appear multiple times in the list, but only the last
        self.suspected_resources.bind_group_layouts.clear();

        self
    }

    fn triage_suspected_query_sets(&mut self, trackers: &Mutex<Tracker<A>>) -> &mut Self {
        let mut trackers = trackers.lock();
        let resource_map = &mut self.suspected_resources.query_sets;
        Self::triage_resources(
            resource_map,
            self.active.as_mut_slice(),
            &mut trackers.query_sets,
            |maps| &mut maps.query_sets,
        );
        self
    }

    fn triage_suspected_staging_buffers(&mut self) -> &mut Self {
        self.suspected_resources.staging_buffers.clear();

        self
    }

    /// Identify resources to free, according to `trackers` and `self.suspected_resources`.
    ///
    /// Remove from `trackers`, the [`Tracker`] belonging to same [`Device`] as
    /// `self`, each resource mentioned in [`self.suspected_resources`]. If
    /// `trackers` held the final reference to that resource, add it to the
    /// appropriate free list, to be destroyed by the hal:
    ///
    /// -   Add resources used by queue submissions still in flight to the
    ///     [`last_resources`] table of the last such submission's entry in
    ///     [`self.active`]. When that submission has finished execution. the
    ///     [`triage_submissions`] method will remove from the tracker and the
    ///     resource reference count will be responsible carrying out deallocation.
    ///
    /// ## Entrained resources
    ///
    /// This function finds resources that are used only by other resources
    /// ready to be freed, and adds those to the free lists as well. For
    /// example, if there's some texture `T` used only by some texture view
    /// `TV`, then if `TV` can be freed, `T` gets added to the free lists too.
    ///
    /// Since `wgpu-core` resource ownership patterns are acyclic, we can visit
    /// each type that can be owned after all types that could possibly own
    /// it. This way, we can detect all free-able objects in a single pass,
    /// simply by starting with types that are roots of the ownership DAG (like
    /// render bundles) and working our way towards leaf types (like buffers).
    ///
    /// [`Device`]: super::Device
    /// [`self.suspected_resources`]: LifetimeTracker::suspected_resources
    /// [`last_resources`]: ActiveSubmission::last_resources
    /// [`self.active`]: LifetimeTracker::active
    /// [`triage_submissions`]: LifetimeTracker::triage_submissions
    pub(crate) fn triage_suspected(&mut self, trackers: &Mutex<Tracker<A>>) {
        profiling::scope!("triage_suspected");

        //NOTE: the order is important to release resources that depends between each other!
        self.triage_suspected_render_bundles(trackers);
        self.triage_suspected_compute_pipelines(trackers);
        self.triage_suspected_render_pipelines(trackers);
        self.triage_suspected_bind_groups(trackers);
        self.triage_suspected_pipeline_layouts();
        self.triage_suspected_bind_group_layouts();
        self.triage_suspected_query_sets(trackers);
        self.triage_suspected_samplers(trackers);
        self.triage_suspected_staging_buffers();
        self.triage_suspected_texture_views(trackers);
        self.triage_suspected_textures(trackers);
        self.triage_suspected_buffers(trackers);
        self.triage_suspected_destroyed_buffers();
        self.triage_suspected_destroyed_textures();
    }

    /// Determine which buffers are ready to map, and which must wait for the
    /// GPU.
    ///
    /// See the documentation for [`LifetimeTracker`] for details.
    pub(crate) fn triage_mapped(&mut self) {
        if self.mapped.is_empty() {
            return;
        }

        for buffer in self.mapped.drain(..) {
            let submit_index = buffer.info.submission_index();
            log::trace!(
                "Mapping of {:?} at submission {:?} gets assigned to active {:?}",
                buffer.info.id(),
                submit_index,
                self.active.iter().position(|a| a.index == submit_index)
            );

            self.active
                .iter_mut()
                .find(|a| a.index == submit_index)
                .map_or(&mut self.ready_to_map, |a| &mut a.mapped)
                .push(buffer);
        }
    }

    /// Map the buffers in `self.ready_to_map`.
    ///
    /// Return a list of mapping notifications to send.
    ///
    /// See the documentation for [`LifetimeTracker`] for details.
    #[must_use]
    pub(crate) fn handle_mapping(
        &mut self,
        raw: &A::Device,
        trackers: &Mutex<Tracker<A>>,
        snatch_guard: &SnatchGuard,
    ) -> Vec<super::BufferMapPendingClosure> {
        if self.ready_to_map.is_empty() {
            return Vec::new();
        }
        let mut pending_callbacks: Vec<super::BufferMapPendingClosure> =
            Vec::with_capacity(self.ready_to_map.len());

        for buffer in self.ready_to_map.drain(..) {
            let tracker_index = buffer.info.tracker_index();
            let is_removed = {
                let mut trackers = trackers.lock();
                trackers.buffers.remove_abandoned(tracker_index)
            };
            if is_removed {
                *buffer.map_state.lock() = resource::BufferMapState::Idle;
                log::trace!("Buffer ready to map {tracker_index:?} is not tracked anymore");
            } else {
                // This _cannot_ be inlined into the match. If it is, the lock will be held
                // open through the whole match, resulting in a deadlock when we try to re-lock
                // the buffer back to active.
                let mapping = std::mem::replace(
                    &mut *buffer.map_state.lock(),
                    resource::BufferMapState::Idle,
                );
                let pending_mapping = match mapping {
                    resource::BufferMapState::Waiting(pending_mapping) => pending_mapping,
                    // Mapping cancelled
                    resource::BufferMapState::Idle => continue,
                    // Mapping queued at least twice by map -> unmap -> map
                    // and was already successfully mapped below
                    resource::BufferMapState::Active { .. } => {
                        *buffer.map_state.lock() = mapping;
                        continue;
                    }
                    _ => panic!("No pending mapping."),
                };
                let status = if pending_mapping.range.start != pending_mapping.range.end {
                    log::debug!("Buffer {tracker_index:?} map state -> Active");
                    let host = pending_mapping.op.host;
                    let size = pending_mapping.range.end - pending_mapping.range.start;
                    match super::map_buffer(
                        raw,
                        &buffer,
                        pending_mapping.range.start,
                        size,
                        host,
                        snatch_guard,
                    ) {
                        Ok(ptr) => {
                            *buffer.map_state.lock() = resource::BufferMapState::Active {
                                ptr,
                                range: pending_mapping.range.start
                                    ..pending_mapping.range.start + size,
                                host,
                            };
                            Ok(())
                        }
                        Err(e) => {
                            log::error!("Mapping failed: {e}");
                            Err(e)
                        }
                    }
                } else {
                    *buffer.map_state.lock() = resource::BufferMapState::Active {
                        ptr: std::ptr::NonNull::dangling(),
                        range: pending_mapping.range,
                        host: pending_mapping.op.host,
                    };
                    Ok(())
                };
                pending_callbacks.push((pending_mapping.op, status));
            }
        }
        pending_callbacks
    }
}
