// We need to implement RenderElement manually due to AsRemoteFrame requirement.
// This macro does it for us.
#[macro_export]
macro_rules! niri_render_elements {
    // The two callable variants: with <R> and without <R>. They include From impls because nested
    // repetitions ($type and $variant with + and $R with ?) don't work properly.
    ($name:ident<R> => { $($variant:ident = $type:ty),+ $(,)? }) => {
        $crate::niri_render_elements!(@impl $name () ($name<R>) => { $($variant = $type),+ });

        $(impl<R: $crate::render_helpers::renderer::NiriRenderer> From<$type> for $name<R> {
            fn from(x: $type) -> Self {
                Self::$variant(x)
            }
        })+
    };

    ($name:ident => { $($variant:ident = $type:ty),+ $(,)? }) => {
        $crate::niri_render_elements!(@impl $name ($name) () => { $($variant = $type),+ });

        $(impl From<$type> for $name {
            fn from(x: $type) -> Self {
                Self::$variant(x)
            }
        })+
    };

    // The internal variant that generates most of the code. $name_no_R and $name_R are necessary
    // for the impl RenderElement<SomeRenderer> for $name<SomeRenderer>: since $R does not appear
    // in this line, we cannot condition based on $R like elsewhere, so we condition on duplicate
    // names instead. Like this: $($name_R<SomeRenderer>)? $($name_no_R)? so only one is chosen.
    (@impl $name:ident ($($name_no_R:ident)?) ($($name_R:ident<$R:ident>)?) => { $($variant:ident = $type:ty),+ }) => {
        #[allow(clippy::large_enum_variant)]
        #[derive(Debug)]
        pub enum $name$(<$R: $crate::render_helpers::renderer::NiriRenderer>)? {
            $($variant($type)),+
        }

        impl$(<$R: $crate::render_helpers::renderer::NiriRenderer>)? smithay::backend::renderer::element::Element for $name$(<$R>)? {
            fn id(&self) -> &smithay::backend::renderer::element::Id {
                match self {
                    $($name::$variant(elem) => elem.id()),+
                }
            }

            fn current_commit(&self) -> smithay::backend::renderer::utils::CommitCounter {
                match self {
                    $($name::$variant(elem) => elem.current_commit()),+
                }
            }

            fn geometry(&self, scale: smithay::utils::Scale<f64>) -> smithay::utils::Rectangle<i32, smithay::utils::Physical> {
                match self {
                    $($name::$variant(elem) => elem.geometry(scale)),+
                }
            }

            fn transform(&self) -> smithay::utils::Transform {
                match self {
                    $($name::$variant(elem) => elem.transform()),+
                }
            }

            fn src(&self) -> smithay::utils::Rectangle<f64, smithay::utils::Buffer> {
                match self {
                    $($name::$variant(elem) => elem.src()),+
                }
            }

            fn damage_since(
                &self,
                scale: smithay::utils::Scale<f64>,
                commit: Option<smithay::backend::renderer::utils::CommitCounter>,
            ) -> smithay::backend::renderer::utils::DamageSet<i32, smithay::utils::Physical> {
                match self {
                    $($name::$variant(elem) => elem.damage_since(scale, commit)),+
                }
            }

            fn opaque_regions(&self, scale: smithay::utils::Scale<f64>) -> smithay::backend::renderer::utils::OpaqueRegions<i32, smithay::utils::Physical> {
                match self {
                    $($name::$variant(elem) => elem.opaque_regions(scale)),+
                }
            }

            fn alpha(&self) -> f32 {
                match self {
                    $($name::$variant(elem) => elem.alpha()),+
                }
            }

            fn kind(&self) -> smithay::backend::renderer::element::Kind {
                match self {
                    $($name::$variant(elem) => elem.kind()),+
                }
            }

            fn is_framebuffer_effect(&self) -> bool {
                match self {
                    $($name::$variant(elem) => elem.is_framebuffer_effect()),+
                }
            }
        }

        impl smithay::backend::renderer::element::RenderElement<$crate::gpu::remote::RemoteRenderer>
            for $($name_R<$crate::gpu::remote::RemoteRenderer>)? $($name_no_R)?
        {
            fn capture_framebuffer(
                &self,
                frame: &mut $crate::gpu::remote::RemoteFrame<'_, '_>,
                src: smithay::utils::Rectangle<f64, smithay::utils::Buffer>,
                dst: smithay::utils::Rectangle<i32, smithay::utils::Physical>,
                cache: &smithay::utils::user_data::UserDataMap,
            ) -> Result<(), $crate::gpu::remote::RemoteError> {
                match self {
                    $($name::$variant(elem) => {
                        smithay::backend::renderer::element::RenderElement::<$crate::gpu::remote::RemoteRenderer>::capture_framebuffer(elem, frame, src, dst, cache)
                    })+
                }
            }

            fn draw(
                &self,
                frame: &mut $crate::gpu::remote::RemoteFrame<'_, '_>,
                src: smithay::utils::Rectangle<f64, smithay::utils::Buffer>,
                dst: smithay::utils::Rectangle<i32, smithay::utils::Physical>,
                damage: &[smithay::utils::Rectangle<i32, smithay::utils::Physical>],
                opaque_regions: &[smithay::utils::Rectangle<i32, smithay::utils::Physical>],
                cache: Option<&smithay::utils::user_data::UserDataMap>,
            ) -> Result<(), $crate::gpu::remote::RemoteError> {
                match self {
                    $($name::$variant(elem) => {
                        smithay::backend::renderer::element::RenderElement::<$crate::gpu::remote::RemoteRenderer>::draw(elem, frame, src, dst, damage, opaque_regions, cache)
                    })+
                }
            }

            fn underlying_storage(&self, renderer: &mut $crate::gpu::remote::RemoteRenderer) -> Option<smithay::backend::renderer::element::UnderlyingStorage<'_>> {
                match self {
                    $($name::$variant(elem) => elem.underlying_storage(renderer)),+
                }
            }
        }

    };
}
