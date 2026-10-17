use extensions::{
	ExtensionKind, MAX_SURFACE_BYTES, Manifest, Package, SurfaceFit, SurfaceImage, Theme,
	ThemeSurface, parse_package,
};

fn theme(surface_images: Vec<(ThemeSurface, Vec<u8>)>, settings: SurfaceImage) -> Package {
	Package {
		background_image: Vec::new(),
		cover_image: Vec::new(),
		surface_images: surface_images.into_iter().collect(),
		manifest: Manifest {
			api_version: 1,
			id: "bars".into(),
			name: "Bars".into(),
			version: "1.0.0".into(),
			author: "Test".into(),
			license: "CC0-1.0".into(),
			source: "https://example.com/bars".into(),
			kind: ExtensionKind::Theme,
			capabilities: Vec::new(),
			actions: Vec::new(),
		},
		theme: Some(Theme {
			surfaces: [(ThemeSurface::CallStage, settings)].into(),
			..Default::default()
		}),
		wasm: Vec::new(),
	}
}

#[test]
fn bar_images_round_trip_and_stay_bounded() {
	let settings = SurfaceImage {
		opacity: 40,
		fit: SurfaceFit::Tile,
		focus: [10, 90],
	};
	let package = theme(vec![(ThemeSurface::CallStage, vec![1, 2, 3])], settings);
	let bytes = serde_json::to_vec(&package).unwrap();
	assert!(String::from_utf8_lossy(&bytes).contains("\"call_stage\""));
	let parsed = parse_package(&bytes).unwrap();
	assert_eq!(
		parsed.surface_images[&ThemeSurface::CallStage],
		vec![1, 2, 3]
	);
	assert_eq!(
		parsed.theme.unwrap().surfaces[&ThemeSurface::CallStage],
		settings
	);

	// Themes without bar images keep their old wire shape.
	let plain = theme(Vec::new(), settings);
	let mut plain_theme = plain.theme.clone().unwrap();
	plain_theme.surfaces.clear();
	assert!(
		!serde_json::to_string(&plain_theme)
			.unwrap()
			.contains("surfaces")
	);
	assert!(
		!serde_json::to_string(&plain)
			.unwrap()
			.contains("surface_images")
	);

	let oversized = theme(
		vec![(ThemeSurface::Composer, vec![0; MAX_SURFACE_BYTES + 1])],
		settings,
	);
	assert!(oversized.validate().is_err());
	let empty = theme(vec![(ThemeSurface::Composer, Vec::new())], settings);
	assert!(empty.validate().is_err());
	for invalid in [
		SurfaceImage {
			opacity: 101,
			..settings
		},
		SurfaceImage {
			focus: [50, 101],
			..settings
		},
	] {
		assert!(theme(Vec::new(), invalid).validate().is_err());
	}

	let mut plugin = theme(vec![(ThemeSurface::Composer, vec![1])], settings);
	plugin.manifest.kind = ExtensionKind::Plugin;
	plugin.theme = None;
	assert!(plugin.validate().is_err());
}
