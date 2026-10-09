// The glyphs of the containers layout's panels, as stroked paths on a
// 24px grid. Layouts have their own (LAYOUT_ICONS in containerTree.ts).
export const PANEL_ICONS = {
  card: "M5 3h14v18H5zM8 8h8M8 12h8M8 16h5",
  solidified: "M5 11h14v10H5zM8 11V7a4 4 0 0 1 8 0v4",
  left: "M19 12H5M11 6l-6 6 6 6",
  right: "M5 12h14M13 6l6 6-6 6",
  up: "M12 19V5M6 11l6-6 6 6",
  down: "M12 5v14M6 13l6 6 6-6",
  wrap: "M3 3h18v18H3zM8 8h8v8H8z",
  takeOut: "M8 8h8v8H8zM3 3l4 4M21 3l-4 4M3 21l4-4M21 21l-4-4",
  rename: "M4 20h4L19 9l-4-4L4 16z",
  save: "M6 3h12v18l-6-4-6 4z",
  reset: "M4 4v6h6M5 15a8 8 0 1 0 2-8.5L4 10",
  pin: "M9 3h6l-1 6 3 3v2H7v-2l3-3zM12 14v7",
  close: "M6 6l12 12M18 6L6 18",
} as const;
