// The stylesheet a root that holds a plot needs: uPlot's own layout
// rules (the canvas, the cursor, the selection) and the app's
// (plot.css). Neither a card's shadow root nor a document's frame sees
// the page's styles, so a card puts this in its `styleSources` and the
// frame in its own stylesheet.
import uplotCss from "uplot/dist/uPlot.min.css?inline";
import plotCss from "./plot.css?inline";

export const PLOT_CSS = `${uplotCss}\n${plotCss}`;
