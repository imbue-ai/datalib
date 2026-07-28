// Pure UI functions

function h(unsafe) {
    if (unsafe == null || unsafe == undefined || (typeof unsafe == 'object' && Object.keys(unsafe).length == 0)) return '';
    if (!(typeof unsafe === 'string' || unsafe instanceof String)) {
        unsafe = unsafe.toString();
    }
    return(unsafe.replaceAll('&(?![a-zA-Z]+\;)', '&amp;').replaceAll('<', '&lt;').replaceAll('>', '&gt;').replaceAll('"', '&quot;').replaceAll("'", '&#039;'));
}

function hx(unsafe) {
    if (unsafe.match(/^_\d/)) {
        return h(unsafe.slice(1));
    } else {
        return h(unsafe);
    }
}

function qs(selector) {
    return document.querySelector(selector)
}

function qsa(selector) {
    return document.querySelectorAll(selector)
}

function afqsa(selector) {
    return Array.from(document.querySelectorAll(selector))
}

function byid(id) {
    return document.getElementById(id)
}

function makeElement(tag, parent, html, cssclasses=[]) {
    e = document.createElement(tag);
    e.innerHTML = html;
    if (cssclasses && cssclasses.length > 0) {
        for (let c=0; c<cssclasses.length; c++) {
            if (cssclasses[c] && cssclasses[c].length > 0) {
                e.classList.add(cssclasses[c]);
            }
        }
    }
    if (parent) {
        parent.append(e);
    }
    return(e);
}

function mmss(ms) {
    seconds = Math.round(ms / 1000);
    minutes = Math.floor(seconds / 60);
    extraseconds = seconds - (60 * minutes);
    return(minutes + ':' + extraseconds.toString().padStart(2, 0));
}

function urlplus(kv) {
    let url = new URL(window.location);
    url.hash = '';
    Object.entries(kv).filter(([k, v]) => k != null && v != null).forEach(([k, v]) => {
        url.searchParams.delete(k);
        url.searchParams.set(k, v);
    });
    return url;
}

function makejsonelement(itemarrayname, box, query=null, queryname=null) {
    const ql = query ? '&middot; <a href="' + urlplus({query: query, queryname: queryname}) + '" target=dactal class=fadelink>link to this query</a>' : '';
    return makeElement('div', box, '<span class="jsontoggle fade" onclick="openjson(' + itemarrayname + ')">data for these results</span>' + ql, ['jsonbox']);
}

function openjson(data, filename='dactal_export') {
    function excluder(key, val) {return (val instanceof HTMLElement || ['available_markets', 'external_urls'].includes(key)) ? undefined: val};
    const w = window.open();
    w.document.write(`
    <!DOCTYPE html>
    <html>
    <head>
        <title>Dactal export</title>
        <style>
            .download-btn {
                position: fixed;
                top: 8px;
                right: 8px;
                cursor: pointer;
            }
        </style>
    </head>
    <body>
        <button class="download-btn" onclick="download()">download</button>
        <pre>${h(JSON.stringify(data, excluder, 4))}</pre>
        <script>
            function download() {
                const blob = new Blob([${JSON.stringify(JSON.stringify(data, excluder, 4))}], 
                    { type: 'application/json' });
                const url = URL.createObjectURL(blob);
                const a = document.createElement('a');
                a.href = url;
                a.download = '${filename}.json';
                a.click();
                URL.revokeObjectURL(url);
            }
        </` + `script>
    </body>
    </html>
    `);
    w.document.close();
}

function escapeAttr(s) {
  return String(s)
    .replace(/&/g, "&amp;")
    .replace(/"/g, "&quot;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;");
}

function sortform(name) {
    temp = name ? name.toString().toLowerCase() : '';
    if (temp.startsWith('the ')) {
        temp = temp.substring(4);
    }
    temp = temp.replaceAll(/ & /g, ' and ');
    return temp
}

var dactalintro = `<div class=dactalnotes>
This is the demonstration query interface for the data-agnostic collation/transformation/analysis language called DACTAL.<br>
&nbsp;<br>
A typical DACTAL query starts with a datatype, like the above. That gets the list of all the things of that type. From there a query can chain of any number of any of these operations, in any order:<br>
&nbsp;<br>
<b>.</b> &nbsp; follow a property from all those things to other things, like <span class=queryexample onclick="runQuery('tracks.artist')">tracks.artist</span><br>
<b>:</b> &nbsp; filter the list of things by some criteria, like <span class=queryexample onclick="runQuery('tracks:artist=Nightwish')">tracks:artist=Nightwish</span><br>
<b>/</b> &nbsp; group the things, like <span class=queryexample onclick="runQuery('tracks/artist')">tracks/artist</span><br>
<b>#</b> &nbsp; sort the things, like <span class=queryexample onclick="runQuery('tracks#artists,album,-disc_number,-track_number')">tracks#artists,album,-disc_number,-track_number</span><br>
</div>

<div class=dactalnotes>DACTAL can also <span onclick="runQuery('[catalog.json].fetch.data|☑︎@')">integrate external-API data or other exploration tools</span>.</div>`;

const dactal_css = `
    #query {display: block}
    button.current, .current :not(.internalsep) {font-weight: bold}
    .note {color: gray}
    .spanlink, span[onclick]:not(.fade, .fadelink, .nolink, .hidecolumn, .overridden) {color: teal}
    .fade, .fade a, .fadelink, .artist_playlist_reference {color: silver}
    .stat {color: gray; text-align: right; white-space: nowrap}
    .elided {white-space: nowrap}
    .column {padding-right: 2px}
    .deletetype, .exporttype, .hidecolumn, .unhidecolumn, .sortcolumn, .filtercolumn, .groupcolumn, .copycolumn {color: gray; padding-left: 2px; visibility: hidden}
    .hidecolumn, .sortcolumn, .filtercolumn, .groupcolumn, .copycolumn {color: #FFD0D0 !important}
    .sortcolumn, .filtercolumn, .groupcolumn, .copycolumn {padding: 0px 2px 0px 2px}
    .sortcolumn.current, .groupcolumn.current {visibility: visible; font-weight: bold; color: white}
    .without {color: silver; font-weight: normal; padding-left: 16px}
    .typewrapper:hover > :is(.deletetype, .exporttype), .queryheader:hover > :is(.hidecolumn, .sortcolumn, .filtercolumn, .groupcolumn, .copycolumn), .hidden:hover > .unhidecolumn {visibility: visible}
    .groupcell {background: #F8F8F8; color: #008080c4; font-weight: bold}
    .datarow {scroll-margin-left: 32px}
    .queryresultstable:has(.groupsclosed) .datarow.more {display: none}
    .queryresultstable:has(.groupstat) :is(.groupcolumn, .sortcolumn:not(.groupsort)) {display: none}
    td.groupstat {white-space: nowrap}
    td.groupstat.groupsclosed {background: red}
    td.groupstat.groupsclosed :is(.groupcloser, .ungroup, .groupsort):hover {color: #FFD0D0 !important}
    .ungroup, .groupsort {visibility: hidden}
    .groupstat:hover :is(.ungroup, .groupsort) {visibility: visible}
    .queryresultstable:has(.groupsclosed) > .grouprow + .datarow + .datarow + .datarow + .datarow > td {border-bottom-color: red}
    :is(span[onclick]:not(.nolink), .fadelink, .otherlink, .rewinder, .rewindstep, .groupcloser, .ungroup, .groupsort):hover {color: red !important; text-decoration: underline; cursor: pointer}
    .nolink:hover {cursor: pointer}
    .rewinder {margin-right: 8px}
    .typewrapper {margin-right: 8px}
    .metadataset {color: silver}
    .jsonbox {color: silver; margin: 32px 0px 0px 16px}
    .jsontoggle {}
    #dactal, .queryta {width: 800px; field-sizing: content}
    #dactal.mono, .queryta.mono {font-family: "Andale Mono"; font-size: 85%}
    #pageheader:has(#otherbuttons:empty) #querybutton {display: none}
    body.as_application #pageheader:not(:has(.pagebutton)) #querybutton {display: none}
    .querybutton, .debugbutton {vertical-align: top; margin-left: 4px}
    #dactalouterbox:has(.queryoutput:empty) .debugbutton, .debugbutton.na {display: none}
    .queryoutputx {white-space: pre}
    .querylabel {display: inline-flex; flex-flow: column; vertical-align: top}
    .querylink, .querylinkdisabled, .querytitle {color: gray; width: 42px; vertical-align: top}
    .queryinput {}
    body:has(.queryinput) .seequery {display: none}
    #afterquerybox {margin: 16px 0px}
    #afterquerybox:empty {display: none}
    .queryresults, #afterquerybox.queryresults {margin: 16px}
    .queryresults .resultslist, .resultslistmore {margin: 16px 0px}
    .queryresultheader {font-weight: bold; white-space: normal}
    .querypending {color: silver}
    .querystarts {margin: 16px 0px 16px 42px; width: 1000px}
    .queryheaderrow {position: sticky; top: 0px; background: white}
    .queryheader, .heatmapheader, .heatmaprowtitle {background: #00808094; color: white; font-weight: bold}
    :is(.queryheader, .heatmapheader, .heatmaprowtitle) span[onclick] {color: white}
    .queryheader.stat {white-space: nowrap}
    .buildedit {display: block}
    .querypath {color: gray}
    .querydata .queryheader {background: #60c0c094}
    .queryresults td {padding: 2px 4px; border: 1px solid silver}
    .queryresults tr:hover > :is(td.querydata, td.stat:not(.queryheader)) {background: #F0F0F0}
    .queryresults tr > td.querydata tr:hover > td.querydata {background: #E4E4E4}
    .queryresults tr > td.querydata tr:hover td.querydata tr:hover > td.querydata {background: #D8D8D8}
    .queryresults tr > td.querydata tr:hover td.querydata tr:hover td.querydata tr:hover > td.querydata {background: #CBCBCB}
    .queryresults tr {vertical-align: top}
    .excluder {visibility: hidden; padding-right: 2px}
    .queryresults tr td:first-child:hover .excluder {visibility: visible}
    .dactalmultiheader {font-weight: bold; color: gray; margin: 8px 0px 4px 2px}
    .dactalviewbox :is(.dactalmultibox:first-child, .dactalmultihorizontal) .dactalmultiheader {margin-top: 16px}
    .dactalviewbox :has(.dactalmultihorizontal) {display: flex; flex-flow: row}
    .dactalmultihorizontal {display: inline-block; vertical-align: top}
    .dactalmultibox + .dactalmultibox:not(.dactalmultihorizontal) {margin-top: 32px}
    .dactalmultihorizontal + .dactalmultihorizontal {margin-left: 32px}
    .dactalmultiboxhorizontalcontainer {white-space: nowrap}
    :is(#query, #box, #output):has(.queryheader, .querydata, .heatmap, .groupcell, .resultslist, .resultscloud, .dactalviewbox) .querystarts {display: none}
    .cellcount {color: silver; float: right; padding-left: 2px}
    .queryresults .jsonbox {margin-left: 0px}
    .queryresults img:not([height][width]) {max-height: 100px}
    .fliprow {width: 100%}
    .fieldframe {width: 800px; height: 200px; border: 1px solid gray; background: #F4F4F4}
    .dactalnotes {color: gray; margin: 32px 0px}
    .savedqueries {margin: 16px 0px}
    .savedqueryheading {color: gray; border-bottom: 1px solid silver; padding-bottom: 4px; margin-bottom: 2px}
    .queryresultcount {color: #d8d8d8; padding-left: 8px}
    #dactalsave {min-width: 150px; margin: -2px 0px 0px 16px; vertical-align: top; field-sizing: content; padding-right: 8px; height: 26px}
    #dactalsavebutton, .queryactionbutton {margin: -2px 0px 0px 2px; vertical-align: top; height: 26px}
    .deletequery, .exportquery, .runorder {visibility: hidden; padding-left: 8px; color: gray}
    .deletequery + .exportquery, .exportquery + .runorder {padding-left: 2px}
    .savedquery {padding-bottom: 4px}
    .savedquery:hover :is(.deletequery, .exportquery, .runorder), .savedquerytag:hover :is(.deletequery, .exportquery, .runorder) {visibility: visible}
    .savedquerytag {color: gray; margin: 16px 0px 8px 0px; border-bottom: 1px solid silver}
    .savedquery:not(.open) .querylong, .savedquery.open .queryshort {display: none}
    .savedquery:not(.open) .queryshort, .savedquery.open .querylong {display: inline}
    .adapters {color: gray; margin-bottom: 16px}
    .queryexample {font-weight: bold}
    .queryresults .stat {text-align: right; color: silver}
    .queryheader.stat {color: white}
    .querystring {color: orange}
    .adaptercell {white-space: nowrap}
    .expanddots {color: gray}
    .longlist div:first-child, .longlist.expanded div {display: block;}
    .longlist div, .longlist.expanded div:first-child, .shortlist div:first-child {display: none;}
    #fileInput {display: none}
    input[type="checkbox"] {margin: 2px 0px 0px 0px}
    .hidden {color: silver; font-weight: normal; padding-left: 8px}
    .heatmaplegend {margin-top: 8px; color: gray}
    .heatmapcell {min-height: 16px; min-width: 16px}
    .heatmap td {text-align: right}
    #tablemargin {display: inline-block; position: sticky; float: left; top: 0; left: 0; margin-left: -32px; width: 32px; min-height: 400px;}
    #rowskipper {display: none; padding: 0px 8px; border: 1px solid silver; margin: 2px}
    #rowskipper:hover, #tablemargin:hover #rowskipper {display: inline-block}
    .resultscloud {border: 1px solid black; margin: 16px 0px; padding: 8px}
    #afterquerybox td:is(.testquery, .note, .code) {padding-left: 8px; padding-right: 8px}

    .gallerytoggle {padding-left: 16px; font-weight: normal; color: silver; display: none}
    .nextquery {padding-left: 8px; font-weight: normal}
    .queryoutput:has(td img, td.name) .gallerytoggle {display: inline-block}
    .queryoutput.gallery:has(td img, td.name) {
        .gallerytoggle {font-weight: bold; color: gray}
        img {padding: 2px}
        tr:has(img, .name):not(.grouprow) {display: inline-flex; flex-flow: column}
        td:has(img, .name) {display: inline; border: none; padding: 0px; order: 1}
        div > table > tr > td.name {
            display: inline;
            border: none;
            padding: 2px 16px 16px 2px;
            order: 2;
            width: 0;
            min-width: 100%;
            word-wrap: break-word;
            box-sizing: border-box;
        }
        tr:not(:has(img)) .name {width: auto !important; padding-bottom: 4px !important}
        tr:not(:has(img, .name)):not(.grouprow), td:not(:has(img)):not(.groupcell) {display: none}
        table {border-spacing: 0px; margin: 0px}
        > div > table {margin: 0px 0px 32px 0px}
        .grouprow {display: block; margin: 4px 0px}
        .groupcell {display: block}
        :is(.shortlist, .longlist):has(img) div:has(img) {display: inline; border: none; padding: none; order: 1}
        :is(.shortlist, .longlist) img {padding: 1px; height: 50px; width: 50px}
        :is(.shortlist, .longlist) :is(br, .expanddots), .cellcount {display: none}
        #tablemargin {display: none}
    }
    
    .hiddenqueryheader {display: none; margin-left: -16px}
    .queryresults > .showquery {display: none}
    .updatedata {padding-left: 16px}
    .updatedata:not([onclick]) {display: none}
    #output > .hiddenqueryheader, .dactalmodulebox .hiddenqueryheader {margin-bottom: 16px}
    .queryhidden {
        .queryinput, .afterquery, .querystarts, .queryresultheader, .querytime {display: none}
        .hiddenqueryheader, .queryresults > .showquery {display: block}
    }
    .filterlist {background: #e0f5f5; border: 1px solid #693f3f; padding: 4px; max-height: 400px; overflow-y: scroll; width: max-content; max-width: 400px; margin: 0}
    .filterval {cursor: pointer}
    .filterval:hover {cursor: pointer; color: red}
    
    .embedquery {
      #output {font: 16px Gill Sans}
      a {color: teal; text-decoration: none}
      a:hover {color: red; text-decoration: underline}
      #dactalsave,
      #dactalsavebutton,
      iframe, 
      .afterquery,
      .querystarts,
      .rewinder, 
      .seequery, 
      .hiddenfields,
      .querytime, 
      .querylabel,
      .querylink,
      .hiddenqueryheader,
      .querybutton,
      .debugbutton,
      .queryresultheader,
      .jsonbox {display: none !important}
      .sortcolumn, .filtercolumn, .groupcolumn, .hidecolumn, .excluder {display: none !important}
      #dactal {margin: 0px 0px 16px 16px; border: none; resize: none; width: auto}
      .otherlink:hover {color: white !important; text-decoration: none !important}
    }
`;

function dactal_ai_init() {
    if ('OPENAI_API_KEY' in localStorage || 'ANTHROPIC_API_KEY' in localStorage) {
        ai = document.createElement('script');
        ai.src = "https://dactal.org/dactal_assist.js";
        if ('OPENAI_API_KEY' in localStorage) {
            ai.addEventListener('load', () => {
                dactal.connect_annotator('ask o3', async (item) => {
                    res = await ask_llm(item.question[0] + ' Return a JSON structure like this: {"question": "...the question here...", "answer": "...the answer here..."}.', JSON.stringify(item.of));
                    return res;
                }, ['question']);
            });
        }
    
        document.head.appendChild(ai);
        document.getElementById('dactal').removeAttribute('onkeydown');
        document.getElementById('dactal').addEventListener('keydown', (e) => queryKeyOverride(e));
        window.addEventListener('heyupdate', async (e) => document.getElementById('afterquerybox').textContent = "Assistant trying: " + e.detail.message);
    }
}

function dactal_params_init() {
    const queryString = window.location.search;
    const urlParams = new URLSearchParams(queryString);
    const dparams = {};
    for (param of ['page', 'query', 'queryname', 'embed', 'reset', 'noindex']) {
        dparams[param] = urlParams.get(param)
    }
    return dparams;
}

async function dactal_datafile_init(dbname=null, resetdata=null) { 
    if (dbname) {
        datafilename = '/data/' + encodeURIComponent(dbname) + '.json';
        try {
            qsdatares = await fetch(datafilename);
            if (qsdatares) {
                qsdata = await qsdatares.json();
                if (qsdata) await import_data(qsdata, resetdata);
                console.log('data imported now');
            }
        } catch (e) {
            console.log('No datafile at ' + datafilename);
            // throw e;
        }
    }
    if (dactal.data.connectors) {
        for (connector of dactal.data.connectors) {
            if (connector.namespace) {
                await loadscript_namespaced(connector.script, connector.namespace);
            } else {
                await loadscript(connector.script);
            }
        }
    }
}

function loadscript(scriptname, initf) {
    return new Promise((resolve, reject) => {
        const s = document.createElement('script');
        s.src = scriptname.startsWith("https://") ? scriptname : "https://dactal.org/" + scriptname;
        s.addEventListener('load', () => {
            console.log({loadedscript: scriptname});
            if (initf) initf();
            resolve(true);
        });
        s.addEventListener('error', () => reject(new Error('Failed to load script ' + scriptname)));
        document.head.appendChild(s);
    });
}

async function loadscript_namespaced(scriptname, namespace) {
    const src = scriptname.startsWith("https://") ? scriptname : "https://dactal.org/" + scriptname;
    const response = await fetch(src);
    if (!response.ok) throw new Error('Failed to fetch ' + src + ' (HTTP ' + response.status + ')');
    const code = await response.text();

    const proxiedDactal = new Proxy(dactal, {
        get(target, prop) {
            if (prop === 'connect') {
                return (key, connectf, doc, annotator) => target.connect(key.startsWith(namespace + ' ') ? key : namespace + ' ' + key, connectf, doc, annotator);
            }
            return target[prop];
        }
    });

    const module = {};
    try {
        new Function('exports', 'dactal', code)(module, proxiedDactal);
    } catch (e) {
        throw new Error('Error running script ' + scriptname + ': ' + e.message);
    }
    console.log({loadedscript: scriptname, namespace: namespace});
    return module;
}

function loadhelp(key, value) {
    dactal.data[key] = value;
    if (!dactal.internal_datasets.includes(key)) dactal.internal_datasets.push(key);
    loadtypes();
}

async function loadpages() {
    byid('otherbuttons').textContent = '';
    byid('otherpages').textContent = '';
    dactal.data.queries?.forEach(async (q) => {
        if (q.tag == 'page queries') {
            const pagebutton = makeElement('button', byid('otherbuttons'), q.name, ['pagebutton']);
            pagebutton.queryname = q.name;
            pagebutton.addEventListener('click', (e) => pickpage(e.target.queryname));
            const pagebox = makeElement('div', byid('otherpages'), '', ['pagebox']);
            pagebox.queryname = q.name;
            // q.results ||= await dactal.query(q.query);
            const res = await dactal_querymodule(q.name);
            pagebox.appendChild(res);
        }
    });
}

function pickpage(page) {
    qsa('.pagebox').forEach((pagebox) => pagebox.style.display = ((!pagebox.queryname && page == 'query') || pagebox.queryname == page ? 'block' : 'none'));
    qsa('.pagebutton').forEach((pagebutton) => pagebutton.classList.toggle('current', (!pagebutton.queryname && page == 'query') || pagebutton.queryname == page));
}

function queryReset() {
    d=document.getElementById('dactal');
    d.value='';
    document.getElementById('queryoutput').textContent = '';
    document.getElementById('afterquerybox').textContent = '';
    let url = new URL(window.location);
    if (url.searchParams.has('query') || url.searchParams.has('queryname')) {
        url.searchParams.delete('query');
        url.searchParams.delete('queryname');
        history.replaceState(null, null, url);
    }
    d.focus()
}

function queryRewind(targetbox) {
    if ('query history' in dactal.data && dactal.data['query history'].length > 1) {
        dactal.data['query history'].shift();
        lastquery = dactal.data['query history'].shift();
        hide = lastquery.hide;
        runQuery(lastquery.query,'',null,targetbox);
    } else {
        queryReset();
    }
}

async function queryKey(e) {
    if (e.target.value.length > 0) {
        e.target.setAttribute('v', e.target.value);
    } else {
        e.target.removeAttribute('v');
    }
    if (e.key == 'Enter' && !e.shiftKey) {
        setTimeout(() => {
            e.target.style.background = '#F0F0F0';
            document.body.style.cursor = 'progress';
        }, 0);
        e.preventDefault();
        await runQuery();
        setTimeout(() => {
            e.target.style.background = '';
            document.body.style.cursor = '';
        }, 1);

    // } else if (e.key == 'Tab') {
    //     e.preventDefault();
    //     const ta = byid('dactal');
    //     const start = ta.selectionStart;
    //     const end = ta.selectionEnd;
    //     ta.value = ta.value.substring(0, start) + '\t' + ta.value.substring(end);
    //     ta.selectionStart = ta.selectionEnd = start + 1;

    } else if (e.key == 'Tab') {
        e.preventDefault();
        const ta = qs('#dactal, .queryta');
        const start = ta.selectionStart;
        const end = ta.selectionEnd;
    
        if (start === end) {
            // No selection: insert tab
            ta.value = ta.value.substring(0, start) + '\t' + ta.value.substring(end);
            ta.selectionStart = ta.selectionEnd = start + 1;
        } else {
            const text = ta.value;
            const beforeSelection = text.substring(0, start);
            const afterSelection = text.substring(end);
    
            const lineStart = beforeSelection.lastIndexOf('\n') + 1;
            let selectionEnd = end;
            if (text[end - 1] === '\n') {
                selectionEnd = end - 1;
            }
            const fullSelection = text.substring(lineStart, selectionEnd);   
             
            if (e.shiftKey) {
                // Shift-Tab: dedent
                const lines = fullSelection.split('\n');
                const canDedent = lines.every(line => line.startsWith('  ') || line.length === 0);
    
                if (canDedent) {
                    const dedentedLines = lines.map(line => {
                        if (line.startsWith('  ')) {
                            return line.substring(2);
                        }
                        return line;
                    });
                    const dedentedText = dedentedLines.join('\n');
                    const trailingNewline = text[end - 1] === '\n' ? '\n' : '';
                    ta.value = text.substring(0, lineStart) + dedentedText + trailingNewline + text.substring(end);
    
                    const newStart = start - (lineStart < start ? 2 : 0);
                    const removedSpaces = fullSelection.length - dedentedText.length;
                    ta.selectionStart = Math.max(lineStart, newStart);
                    ta.selectionEnd = end - removedSpaces;
                }
            } else {
                // Tab: indent
                const lines = fullSelection.split('\n');
                const indentedLines = lines.map(line => '  ' + line);
                const indentedText = indentedLines.join('\n');
                const trailingNewline = text[end - 1] === '\n' ? '\n' : '';
                ta.value = text.substring(0, lineStart) + indentedText + trailingNewline + text.substring(end);
    
                const addedSpaces = indentedText.length - fullSelection.length;
                ta.selectionStart = start + (lineStart < start ? 2 : 0);
                ta.selectionEnd = end + addedSpaces;
            }
        }

    } else if (e.key == 'i' && e.metaKey) {
        const ta = byid('dactal');
        let start = ta.selectionStart;
        let end = ta.selectionEnd;
        let selectedText = ta.value.slice(start, end);
        const returnafter = selectedText.endsWith('\n');
        if (selectedText.trim().startsWith('.')) {
            const selectedProp = selectedText.trim().replace(/^\.*/, '');
            const propQuery = dactal.data.queries.find((q) => q.name == selectedProp);
            if (propQuery?.relative) {
                const inserttext = '\n??? ' + dactal.bracket(selectedProp) + '\n\n' + propQuery.query + (returnafter ? '\n': '');
                document.execCommand('insertText', false, inserttext);
                ta.selectionStart = start;
                ta.selectionEnd = start + inserttext.length;
            }
        } else if (selectedText.trim().startsWith('???')) {
            while (ta.value.slice(start - 2, start) == '\n\n') {
                ta.selectionStart -= 1;
                start -= 1;
            }
            while (ta.value[end] == '\n\n') {
                console.log({bump: end})
                ta.selectionEnd += 1;
                end += 1;
            }
            selectedText = ta.value.slice(start, end);
            const returnafter = selectedText.endsWith('\n');
            const [saveProp, saveQuery] = selectedText.trim().match(/\?\?\? ?(.*?)\n+(.*)$/s).slice(1);
            const saveTarget = dactal.data.queries.find((q) => q.name == saveProp);
            if (saveTarget) {
                saveTarget.query = saveQuery;
            } else {
                newQuery = {name: saveProp, query: saveQuery, relative: true};
                dactal.data.queries.push(newQuery);
                byid('savedqueries').innerHTML = await loadqueries();
            }
            const replacetext = '.' + dactal.bracket(saveProp) + (returnafter ? '\n' : '');
            document.execCommand('insertText', false, replacetext);
            ta.selectionStart = start;
            ta.selectionEnd = start + replacetext.length;
            await dsave('queries');
        }
    } else if (e.key == 'u' && e.metaKey) {
        const ta = byid('dactal');
        let start = ta.selectionStart;
        let end = ta.selectionEnd;
        if (end > start) {
            selection = ta.value.slice(start, end);
            res = await dactal.query(selection);
            if (res?.length >= 0) {
                resstr = res.map((r) => dactal.bracket(dactal.getname(r))).join(',');
                document.execCommand('insertText', false, resstr);  
            }
        }
    }
}

const enterEvent = new KeyboardEvent('keydown', {
    key: 'Enter',
    code: 'Enter',
    keyCode: 13,
    which: 13,
    bubbles: true,
    cancelable: true
});

async function queryButton(recache=false) {
    if (recache) dactal.recache = new Set();
    const textarea = document.getElementById('dactal');
    textarea.focus();
    textarea.dispatchEvent(enterEvent);
}

var query_results = null;
var query_time = null;
var hide = [];

function buildView(data, query, target, inline=false, extendable=true) {
    const build = makeElement('div', null, '', ['dactalviewbox']);
    if (data?.length > 0 && dactal.dtype(data[0], 'object')) {
        if (data.length == 1 && data[0].multi) {
            let multival = data[0].multi;
            if (Array.isArray(multival)) multival = multival[0];
            if (!['horizontal', 'vertical'].includes(multival)) multival = '';
            if (multival == 'horizontal') build.classList.add('dactalmultiboxhorizontalcontainer');
            for (prop of Object.keys(data[0]).filter((prop) => !['of', 'multi'].includes(prop))) {
                const multibox = makeElement('div', build, '', ['dactalmultibox']);
                if (multival) multibox.classList.add('dactalmulti' + multival);
                const multiheader = makeElement('div', multibox, prop, ['dactalmultiheader', 'otherlink']);
                multiheader.setAttribute('query', query + '.' + dactal.bracket(prop));
                multiheader.addEventListener('click', (e) => runQuery(e.target.getAttribute('query')));
                multibox.appendChild(buildView(data[0][prop], query + '.' + dactal.bracket(prop), 'multi', true, false));
            }
        } else if (data.length == 1 && 'xvals' in data[0] && 'yvals' in data[0]) {
            data[0].of.forEach((qr) => delete qr.of);
            build.appendChild(heatmap(data, query));
        } else if ('list' in data[0]) {
            res = makeElement('div', build, '', ['resultslist']);
            renderer = renderers[data[0].list]?.find((r) => r.list);
            for (row of data) {
                if (renderer) {
                    try {
                        rendered = renderer.list(row);
                        res.appendChild(rendered);
                        continue;
                    } catch (e) {console.error(e)}
                }
                makeElement('div', res, dactal.getname(row));
            }
            truncated = query.match(/^(.*?)\?\?\?sample:@(?:<=)?(\d+)$/);
            if (truncated && data.length == Number(truncated[2])) {
                makeElement('div', res, '<span class=fade onclick="runQuery(unescapequery(\'' + escapequery(truncated[1]) + '\'))">more...</span>', ['resultslistmore']);
            }
            build.appendChild(res);
        } else if ('cloud' in data[0]) {
            res = makeElement('div', build, '', ['resultscloud']);
            const maxcloudfont = 120;
            const mincloudfont = 12;
            let maxscore = 0;
            let maxcount = 0;
            let mincount = null;
            data[0].of.forEach((obj) => {
                if (obj?.score > maxscore) maxscore = obj.score;
                if (obj?.count > maxcount) maxcount = obj.count;
                if (!mincount || obj?.count < mincount) mincount = obj.count;
            });
            if (maxscore == 0) maxscore = 1;
            if (maxcount == 0) maxcount = 1;
            if (!mincount) mincount = 0;
            let sizescale = data[0].sizescale;
            if (Array.isArray(sizescale)) sizescale = sizescale[0];
            let colorscale = data[0].colorscale;
            if (Array.isArray(colorscale)) colorscale = colorscale[0];
            data[0].of.forEach((obj, ox) => {
                let objscore = obj.score;
                if (Array.isArray(objscore)) objscore = objscore[0];
                if (objscore == null) objscore = 0;
                const drop = makeElement('span', res, (ox > 0 ? ' ' : '') + h(obj.name));
                const sizeraw = objscore / maxscore;
                const size = sizescale == 'log' ? Math.log(sizeraw + 1) : sizescale == 'square' ? sizeraw ** 2 : sizescale == 'sqrt' ? Math.sqrt(sizeraw) : sizeraw;
                const rednessraw = 256 * (obj.count - mincount) / (maxcount - mincount);
                const redness = colorscale == 'log' ? Math.log(rednessraw + 1) : colorscale == 'square' ? rednessraw ** 2 : colorscale == 'sqrt' ? Math.sqrt(rednessraw) : rednessraw;
                const color = 'rgb(' + Math.round(redness) + ',0,0)';
                drop.style.fontSize = Math.round(mincloudfont + maxcloudfont * size) + 'px';
                drop.style.color = color;
                drop.title = 'count: ' + obj.count + ('score' in obj ? ', score: ' + objscore.toFixed(3) : '');
            });
            build.appendChild(res);
        } else if (query.endsWith('???sample:@1')) {
            makeElement('div', build, render(data[0], query.replace(/\?\?\?sample\:@1$/,'')), ['fliprow']);
        } else {
            build.appendChild(arrayToTable(data, query, inline ? [] : null, extendable));
        }
    } else if (data?.length > 0) {
        build.appendChild(arrayToTable(data, query, inline ? [] : null, extendable));
    } else {
        build.appendChild(arrayToTable([]));
    }
    return build;
    
}

async function runQuery(query=null, queryname=null, results=null, queryoutput=null, inline=false) {
    if (!queryoutput && !document.getElementById("dactal")) await switch_to_querypage();
    const defaultqueryoutput = document.getElementById("queryoutput");
    if (!queryoutput) queryoutput = defaultqueryoutput;
    queryinput = document.getElementById("dactal");
    if (queryinput) {
        if (query) {
            query = query.replaceAll(/\<br\>/g, '\n');
            queryinput.value = query;
        } else {
            query = queryinput.value;
        }
    }

    querynameinput = document.getElementById("dactalsave");
    if (querynameinput) {
        if (queryname != null && queryname != undefined) {
            querynameinput.value = queryname;
        } else {
            queryname = querynameinput.value;
        }
    }
    afterquerybox = document.getElementById('afterquerybox');
    
    if (!query.startsWith('query history') && (!dactal.data['query history'] || dactal.data['query history'].length === 0 || query != dactal.data['query history'][0].query)) {
        dactal.data['query history'] ??= [];
        dactal.data['query history'].unshift({query: query, hide: hide});
        dsave('query history', true);
    }

    queryinput?.classList.toggle('mono', query?.match(/\.\.\.grid|\,align/));
    if (afterquerybox) afterquerybox.textContent = '';
    if (!inline) {
        queryoutput.textContent = '';
        audioshown = 0;
    }
    let queued = [];
    const start = new Date();
    try {
        queryparsed = dactal.parse(query);
        if (results) {
            qpromise = Promise.resolve(results.slice(0));
        } else {
            if (query.match(/\n\?\?\?queue\n/) && !inline) {
                qpromise = runQueue(query);
            } else {
                qpromise = dactal.query(query);
            }
        }
        query_results = await qpromise;
        query_time = new Date() - start;
        
        queued = Object.keys(dactal.adapters).sort().map((adapter) => [adapter, dactal.adapters[adapter].queue.length]).filter(([k, v]) => v > 0);
        if (!inline) {
            queryoutput.textContent = '';
            audioshown = 0;
        }
        res = buildView(query_results, query, queryoutput, inline, queryoutput == defaultqueryoutput);
    }
    catch (error) {
        res = arrayToTable([{error: error.message, trace: '<pre>' + error.stack + '</pre>', cause: error.cause}]);
        queryparsed = [];
    }
    previous_query = '<span class="fade rewinder">&#9198;</span>';
    querysteps = queryparsed.map((q, qx) => '<span class=rewindstep step=' + (qx + 1) + ' title="rewind to this point">' + q.operator + '</span>').join(' ');
    nextquery = null;
    if (queryname) {
        savedquery = dactal.data.queries.find((q) => q.name == queryname);
        if (savedquery && savedquery.order) {
            nextquery = dactal.data.queries.find((q) => q.tag == savedquery.tag && q.order == savedquery.order + 1);
        } 
    }
    gallerylink = '<span class="gallerytoggle fade" onclick="byid(\'queryoutput\').classList.toggle(\'gallery\')">gallery</span>';
    resheader = makeElement('div', queryoutput, previous_query + '<span class=querypath>' + querysteps + '<span class=hiddenfields>' + hiddenfields() + '</span><input type=text id=dactalsave placeholder="name to save..." onkeyup="dactalsave(event)" value="' + (queryname ? escapequery(queryname) : '') + '" v="' + (queryname && queryname.length > 0) + '"><button id=dactalsavebutton onclick="dactalsave(event)"' + (!queryname ? ' disabled' : '') + '>save</button></span>' + (nextquery ? '<span class="fadelink nextquery">&rarr; ' + h(nextquery.name) + '</span>' : '') + gallerylink, ['queryresultheader']);
    resheader.querySelector('.nextquery')?.addEventListener('click', () => runQuery(nextquery.query, nextquery.name, null, queryoutput));
    resheader.querySelector('.rewinder').addEventListener('click', () => {queryRewind(queryoutput)})
    resheader.querySelectorAll('.rewindstep').forEach((x) => x.addEventListener('click', (e) => {
        runQuery(dactal.disassemble(queryparsed.slice(0, Number(e.target.getAttribute('step')))), '');
    }));
    if (queued.length > 0) {
        makeElement('div', queryoutput, 'pending: ' + queued.map(([k, v]) => v + ' ' + k).join(', '), ['querypending']);
    }
    makeElement('div', queryoutput, 'show query', ['showquery', 'fade', 'fadelink']).addEventListener('click', () => document.body.classList.toggle('queryhidden'));
    queryoutput.appendChild(res);
    if (!inline) {
        querylink = makeElement('div', queryoutput, 'see the query for this', ['fade', 'fadelink', 'seequery']);
        querylink.addEventListener('click', () => {runQuery(query)});
        makeElement('div', queryoutput, 'query time: ' + (query_time < 5000 ? query_time + 'ms' : mmss(query_time)), ['querytime', 'fade']);
        makejsonelement('query_results', queryoutput, query, queryname);
    }

    if (dactal.index_modified.size > 0) {
        for (key of dactal.index_modified) await dactaldb.set('_' + key, dactal.index[key]);
        dactal.index_modified = new Set;
    }

    if (afterquerybox) {
        aqres = await afterquery(query_results, query);
        if (aqres) {
            if (aqres instanceof HTMLElement && afterquerybox instanceof HTMLElement) {
                afterquerybox.appendChild(aqres)
            } else {
                afterquerybox.innerHTML = aqres;
            }
        }
    }
    dactal.statusf();
    return query_results;
}

function extendquery(basequery, extension) {
    const usethese = afqsa('.usethese').filter((u) => u.checked);
    const filterstr = (usethese?.length > 0) ? ':' + usethese.map((u) => '@' + u.value).join(',') : '';
    runQuery(basequery + filterstr + (extension ? '.' + extension : ''));
}

function hiddenfields() {
    if (hide?.length === 0) return '';
    return '<span class=without>without:</span>' + hide.map((hf) => '<span class=hidden>' + h(hf) + '<span class=unhidecolumn onclick="unhidecolumn(\'' + escapequery(hf) + '\')">&times;</span></span>').join('');
}

function headertext(data, basequery, field=null, within=null) {
    let rendered;
    if (field in renderers) {
        for (renderer of renderers[field]) {
            if (renderer.header) {
                rendered = renderer.header(data, basequery, field);
                if (rendered != null) {
                    break;
                }
            }
        }
    }
    rendered ??= {text: field ? hx(field) : 'value', opts: ['sort', 'filter', 'group', 'hide', 'copy', 'link']};
    if (rendered.opts.includes('link') && field && basequery) {
        fieldtext = '<span class="column otherlink">' + rendered.text + '</span>';
    } else {
        fieldtext = '<span class=column>' + rendered.text + '</span>';
    }
    if (rendered.opts.includes('filter') && field && basequery) {
        filterer = '<span class="filtercolumn otherlink" title="filter by this column">:</span>';
    } else {
        filterer = '';
    }
    if (rendered.opts.includes('group') && field && basequery) {
        grouper = '<span class="groupcolumn otherlink" title="group by this column">/</span>';
    } else {
        grouper = '';
    }
    if (rendered.opts.includes('sort')) {
        if (field && basequery?.endsWith('#' + dactal.bracket(field)) || basequery?.endsWith('#-' + dactal.bracket(field)) || basequery?.endsWith('#+' + dactal.bracket(field))) {
            sortarrow = '<span class="sortcolumn current otherlink" title="sort by this column">&darr;</span>';
        } else if (basequery) {
            sortarrow = '<span class="sortcolumn otherlink" title="sort by this column">&darr;</span>';
        } else {
            sortarrow = '';
        }
    } else {
        sortarrow = '';
    }
    if (rendered.opts.includes('hide') && field) {
        hidestr = fieldsToPath((within || []).concat([field]));
        hider = '<span class=hidecolumn onclick="hidecolumn(\'' + escapequery(hidestr) + '\')">&times;</span>';
    } else {
        hider = '';
    }
    if (rendered.opts.includes('copy') && !field && query_results && query_results.length > 0 && dactal.dtype(query_results[0], 'literal')) {
        copier = '<span class=copycolumn title="copy this list to the clipboard" onclick="navigator.clipboard.writeText(query_results.join(\'\\n\'))">&#x00A9;</span>';
    } else {
        copier = '';
    }
    return fieldtext + sortarrow + filterer + grouper + copier + hider;
}

function fieldsToPath(fields) {
    return fields.map((f) => dactal.bracket(f)).join('.');
}

var maxUInodes = 10000;
function countNodes(data, limit=maxUInodes) {
    try {
        let n = 0;
        const stack = [data];
        while (stack.length) {
            const val = stack.pop();
            if (++n > limit) return null;
            if (val && typeof val === 'object') {
                stack.push(...Object.entries(val).filter(([k, v]) => k != 'of').map(([k, v]) => v));
            }
        }
        return n;
    } catch (e) {
        console.error(e);
        return null;
    }
}

var maxrows = 1000;
var maxinlinerows = 100;
var maxfields = 100;
var maxinlinefields = 100;
var rowsample = 1;
var buildcolumn = {};

function arrayToTable(dataarray, queryraw, within=null, extendable=true) {
    const minmaxcolor = queryraw?.match(/\?\?\?color/);
    const resultsbox = document.createElement('div');
    if (extendable && !within) {
        tablemargin = makeElement('div', resultsbox, '');
        tablemargin.id = 'tablemargin';
        rowskipper = makeElement('div', tablemargin, '&darr;', ['fade', 'fadelink']);
        rowskipper.id = 'rowskipper';
        rowskipper.title = 'jump the next row into view';
        rowskipper.addEventListener('click', jumprow);
    }
    const resultstable = makeElement('table', resultsbox, '', ['queryresultstable']);
    const query = queryraw?.replace(/\?\?\?sample\:@(<=)?\d+$/, '');
    const queryparsed = dactal.parse(query);
    if (!within) resultstable.id = 'queryresultstable';
    
    if (dataarray && dataarray.length > 0) {
        const usemaxrows = within ? maxinlinerows : maxrows;
        const usemaxfields = within ? maxinlinefields : maxfields;
        let totalrows;
        let totalgroups;
        let datagroups;
        let shownrows;
        let isgrouped = false;
        if (!within && typeof dataarray[0] == 'object' && 'key' in dataarray[0] && 'of' in dataarray[0] && 'heading' in dataarray[0]) {
            isgrouped = true;
            totalgroups = dataarray.length;
            totalrows = dataarray.reduce((acc, g) => acc + g.of.length, 0);
            shownrows = 0;
            datagroups = [];
            for (g of dataarray) {
                if (shownrows + g.of.length <= maxrows || datagroups.length == 0) {
                    datagroups.push(g);
                    shownrows += g.of.length;
                    if (shownrows >= maxrows) break;
                }
            }
        } else {
            totalrows = dataarray.length;
            datagroups = [{of: dataarray.slice(0, usemaxrows)}];
            shownrows = datagroups[0].of.length;
        }
        const headerrow = makeElement('tr', resultstable, '', within ? null : ['queryheaderrow']);
        if (!within) {
            if (isgrouped) {
                const basearray = dataarray.slice(0);
                countcell = makeElement('td', headerrow, '<span class="groupsort headerlink" title="toggle group sorting between name and count">&varr;</span> <span class="groupcloser"><span title="group count">' + datagroups.length + '</span> &middot; <span title="total row count">' + shownrows + '</span></span>', ['queryheader', 'groupstat']);
                if (query) countcell.querySelector('.groupsort').addEventListener('click', () => runQuery(query.endsWith('#count') ? query.slice(0, -6) : query + '#count'));
                countcell.querySelector('.groupcloser').addEventListener('click', (e) => e.target.closest('.groupstat').classList.toggle('groupsclosed'));
            } else {
                makeElement('td', headerrow, '<span title="total row count">' + shownrows + '</span>', ['queryheader', 'stat', 'otherlink']).addEventListener('click', () => runQuery(query + '???sample:@1', null, null, within));
            }
        }
        let allkeys = [];
        let rowcounter = 0;
        for (datagroup of datagroups) {
            for (row of datagroup.of) {
                rowkeys = getkeys(row);
                for (rowkey of rowkeys) {
                    if (allkeys.indexOf(rowkey) == -1) allkeys.push(rowkey);
                }
                // if (rowkeys.length > allkeys.length) allkeys = rowkeys;
                rowcounter += 1;
                if (rowcounter >= rowsample) break;                
            }
        }
        let keys = allkeys.filter((k) => {
            return !k.startsWith('_') && !hide.includes(within ? fieldsToPath(within.concat([k])) : k) && !hide.includes('.' + dactal.bracket(k));
        });
        if (keys.length > usemaxfields) keys = keys.slice(0, usemaxfields);
        for (const field of keys) {
            if (field == '_css') continue;
            headercell = makeElement('td', headerrow, headertext(dataarray, within || isgrouped ? null : query, field, within), ['queryheader']);
            if (!within) {
                const bfield = dactal.bracket(field);
                const lastop = queryparsed[queryparsed.length - 1];
                const lastarg = lastop && lastop.operator == '#' && lastop.args.length == 1 && !lastop.args[0].label && lastop.args[0];
                let sortquery;
                if (lastarg) {
                    sortquery = dactal.disassemble(queryparsed.slice(0, -1)) + '#' + (lastarg.value != field || lastarg.subop == '-' ? '+' : '-') + bfield;
                } else if (field.match(/rank/)) {
                    sortquery = query + '#-' + bfield;
                } else if (isgrouped && query.endsWith('/' + bfield)) {
                    sortquery = query + '#count';
                    headercell.querySelector('.sortcolumn')?.classList.add('groupsort')
                } else if (isgrouped && query.endsWith('/' + bfield + '#count')) {
                    sortquery = query.replace(/#count$/, '');
                    headercell.querySelector('.sortcolumn')?.classList.add('groupsort')
                } else {
                    sortquery = query + '#' + bfield;
                }
                headercell.querySelector('.sortcolumn')?.addEventListener('click', () => {runQuery(sortquery, null, null, null, within)});
                headercell.querySelector('.filtercolumn')?.addEventListener('click', (event) => filtervals(field, query, dataarray, event));
                headercell.querySelector('.groupcolumn')?.addEventListener('click', () => {runQuery(query + '/' + dactal.bracket(field), null, null, null, within)});
                headercell.querySelector('.column')?.addEventListener('click', () => {hide=[];runQuery(query + (isgrouped ? '.of' : '') + '.' + dactal.bracket(field), null, null, null, within)});
                if (field in buildcolumn) {
                    buildop = queryparsed[queryparsed.length - 1];
                    if (buildop.operator == '|' || buildop.operator == '||') {
                        buildarg = buildop.args.find((arg) => arg.label == buildcolumn && Array.isArray(arg.value));
                        if (buildarg) {
                            buildedit = makeElement('input', headercell, '', ['buildedit']);
                            buildedit.type = 'text';
                            buildedit.value = dactal.disassemble(buildarg.value);
                        }
                    }
                }
            }
        }
        if (keys.length === 0) {
            headercell = makeElement('td', headerrow, headertext(dataarray, query), ['queryheader']);
            if (query.endsWith('#')) {
                sortquery = query + '-';
            } else if (query.endsWith('#-')) {
                sortquery = query.replace(/\-$/, '+');
            } else {
                sortquery = query + '#';
            }
            headercell.querySelector('.sortcolumn')?.addEventListener('click', () => {runQuery(sortquery, null, null, null, within)});
        }
        const relevant_adapters = Object.keys(dactal.adapters).filter((key) => {
            if (dactal.adapters[key].annotator) {
                return dactal.adapters[key].annotator.filter((req) => !keys.includes(req)).length == 0;
            } else {
                return keys.length === 0 || keys.includes('id') || keys.includes('value');
            }
        }).sort();
        const relative_queries = dactal.data.queries?.filter((q) => q.relative);
        const showadapters = extendable && !within && datagroups.length == 1 && relevant_adapters.length + relative_queries?.length > 0;
        if (showadapters) {
            makeElement('td', headerrow, '...', ['queryheader', 'adaptercell']);
        }
                
        const minmaxes = {};
        const nonnumeric = new Set();
        if (minmaxcolor) {
            for (datagroup of datagroups) {
                for (datarow of datagroup.of) {
                    for (key of keys) {
                        if (!nonnumeric.has(key)) {
                            const val = datarow[key];
                            if (!isNaN(val)) {
                                const numval = Number(val);
                                if (!(key in minmaxes)) minmaxes[key] = {min: null, max: null};
                                if (!minmaxes[key].min || numval < minmaxes[key].min) minmaxes[key].min = numval;
                                if (!minmaxes[key].max || numval > minmaxes[key].max) minmaxes[key].max = numval;
                            } else {
                                nonnumeric.add(key);
                            }
                        }
                    }
                }
            }
        }

        for (let gi=0; gi<datagroups.length; gi++) {
            datagroup = datagroups[gi];
            dataarray = datagroup.of;
            groupprops = Object.keys(datagroup).filter((prop) => !['name', 'count', 'key', 'of'].includes(prop));
            if (datagroup.heading) {
                grouprow = makeElement('tr', resultstable, '', ['grouprow']);
                groupcell = makeElement('td', grouprow, '<span class="fadelink stat groupindex" title="group ' + (gi + 1) + '">' + (gi + 1) + '</span>&nbsp; ' + datagroup.heading.map((k) => render(dactal.getname(k))).join(' &middot; ') + ' &nbsp;<span class="fadelink stat groupcount" title="group items">' + dataarray.length + '</span>', ['groupcell']);
                groupcell.colSpan = allkeys.length + 1;
                groupcell.querySelector('.groupindex').addEventListener('click', () => runQuery(query + ':@' + (gi+1), null, null, null, within));
                groupcell.querySelector('.groupcount').addEventListener('click', () => runQuery(query + ':@' + (gi+1) + '..of', null, null, null, within));
            }
            for (let i=0; i<dataarray.length; i++) {
                const result = dataarray[i];
                const datarow = makeElement('tr', resultstable, '', ['datarow']);
                if (result.id) {
                    datarow.setAttribute('dataid', result.id);
                    if (queryraw) datarow.setAttribute('dataidquery', queryraw);
                }
                if (i >= 4) datarow.classList.add('more');
                if (!within) {
                    let excluder = null;
                    let excluded = null;
                    for (excludekey of ['id', 'uri', 'name']) {
                        if (typeof result == 'object' && keys.includes(excludekey) && excludekey in result) {
                            let excludeval = result[excludekey];
                            if (dactal.dtype(excludeval, 'array') && excludeval.length == 1) excludeval = excludeval[0];
                            if (dactal.dtype(excludeval, 'literal')) {
                                excluder = '<span class="fade fadelink excluder">&times;</span>';
                                excludesuffix = ':' + excludekey + '!=' + dactal.bracket(excludeval);
                                if (endfilter = query.match(/:@<=\d+/)) {
                                    excluded = query.slice(0, -endfilter[0].length) + excludesuffix + endfilter[0];
                                } else {
                                    excluded = query + excludesuffix;
                                }
                                break;
                            }
                        }
                    }
                    rownumcell = makeElement('td', datarow, (excluder || '') + '<span class="fade fadelink">' + (i+1) + '</span>', ['stat']);
                    if (result.id) {
                        rownumcell.data_id = result.id;
                    } else if (result.name) {
                        rownumcell.data_name = result.name;
                    }
                    if (excluder) rownumcell.firstChild.addEventListener('click', () => {runQuery(excluded || query, null, null, null, within)});
                    rownumcell.lastChild.addEventListener('click', (e) => {
                        if (e.metaKey) {
                            const row = e.target.parentElement;
                            if (row.data_id) {
                                runQuery(query + (isgrouped ? '.of' : '') + ':' + dactal.bracket(row.data_id), null, null, null, within);
                            } else if (row.data_name) {
                                runQuery(query + (isgrouped ? '.of' : '') + ':name=' + dactal.bracket(row.data_name), null, null, null, within);
                            } else {
                                runQuery(query + (isgrouped ? '.of' : ':@=') + (i+1), null, null, null, within)
                            }
                        } else {
                            runQuery(query + (isgrouped ? '.' : ':@<=') + (i+1), null, null, null, within)
                        }
                    });
                }
                if (result && typeof result == 'object') {
                    if (keys.length > 0) {
                        for (const field of keys) {
                            if (field == '_css') {
                                result[field].forEach((cssval) => datarow.classList.add(cssval));
                            } else {
                                const datacell = makeElement('td', datarow, render(result[field], query, field, i+1, (within || []).concat([field])), ['querydata']);
                                if (field in minmaxes && !nonnumeric.has(field)) {
                                    const normalized = 2 * (Number(result[field]) - minmaxes[field].min) / (minmaxes[field].max - minmaxes[field].min);
                                    const red = normalized < 1 ? Math.round(200 * (1 - normalized)) : 0;
                                    const green = normalized > 1 ? Math.round(200 * (normalized - 1)) : 0;
                                    datacell.style.color = 'rgb(' + red + ',' + green + ',0)';
                                }
                                if (['id', 'name'].includes(field)) datacell.classList.add(field);
                            }
                        }
                    } else {
                        const datacell = makeElement('td', datarow, '', ['querydata']);
                    }
                } else if (result && typeof result == 'string' && (result.startsWith('<') || result.match(/<br>/))) {
                    const datacell = makeElement('td', datarow, render(result), ['querydata']);
                } else if (result && typeof result == 'string' && result.match(/^ +$/)) {
                    const datacell = makeElement('td', datarow, '&nbsp;', ['querydata']);
                } else {
                    const datacell = makeElement('td', datarow, h(result), ['querydata']);
                }
                if (i === 0 && showadapters) {
                    const proplist = relevant_adapters.map((key) => ['adapter', key]).concat(relative_queries?.map((q) => ['query', q.name, q.query]));
                    adaptercell = makeElement('td', datarow, proplist.map(([proptype, prop]) => '<div class="fade fadelink" onclick="extendquery(unescapequery(\'' + escapequery(query?.replace(/\|☑︎@$/, "")) + '\'), unescapequery(\'' + escapequery(prop) + '\'))">' + (proptype == 'query' ? '<span class=fade>—.</span>' : '') + h(prop) + '</div>').join('\n'), ['adaptercell']);
                    adaptercell.rowSpan = dataarray.length;
                }
            }
            if (query && !within && !isgrouped) {
                truncated = queryraw.match(/^(.*?)\?\?\?sample:@(?:<=)?(\d+)$/);
                let message = null;
                if (truncated && query_results.length == Number(truncated[2])) {
                    message = '<span class=fade onclick="runQuery(unescapequery(\'' + escapequery(truncated[1]) + '\'))">more...</span>';
                } else if (totalrows > dataarray.length) {
                    message = '<span class=fade>' + dataarray.length + ' of ' + totalrows + ' rows shown</span>';
                }
                if (message) makeElement('tr', resultstable, '<td class=querydata colspan=' + ((keys.length || 1) + 1) + '>' + message + '</td>');
            } else if (gi == datagroups.length - 1 && isgrouped && totalgroups > datagroups.length) {
                makeElement('tr', resultstable, '<td class=querydata colspan=' + ((keys.length || 1) + 1) + '><span class=fade>' + shownrows + ' of ' + totalrows + ' rows shown in ' + datagroups.length + ' of ' + totalgroups + ' groups</span></td>');
            }
        }
    } else {
        makeElement('tr', resultstable, '<td class="fade querydata">no results</td>');
    }
    setTimeout(() => dactal.statusf(), 4000);
    return resultsbox;
}

async function runQueue(query) {
    const [queueq, processq, postq] = query.split(/\?\?\?queue|\?\?\?post/).map((x) => x.trim());
    return new Promise(async (resolve) => {
        const oldlimit = dactal.timelimit;
        const oldrecache = dactal.recache;
        dactal.timelimit = null;
        const queryoutput = byid('queryoutput');
        const queue = await dactal.query(queueq);
        const results = [];
        const resultids = new Set();
        let lastdraw = 0;
        if (oldrecache || !(processq in dactal.index)) {
            dactal.index[processq] = {}
        }
        const loopf = async (x, torecache) => {
            const loopitem = queue[x];
            const loopitemid = dactal.getid(loopitem);
            const loopindex = dactal.index[processq];
            let loopres;
            if (loopindex && loopitemid in loopindex) {
                loopres = loopindex[loopitemid];
                fromcache = true;
            } else {
                dactal.recache = torecache && new Set();
                loopres = await dactal.query(processq, [queue[x]]);
                loopindex[loopitemid] = loopres;
                dactaldb.set('_' + processq, dactal.index[processq]);
            }
            if (loopres?.length > 0) {
                loopres.forEach((lx) => {
                    if (lx) {
                        lxid = dactal.getid(lx);
                        if (!lxid || !resultids.has(lxid)) {
                            results.push(lx);
                            resultids.add(lxid);
                        }
                    }
                });
                const now = performance.now();
                if (now - lastdraw > 1000) {
                    const view = buildView(loopres, queueq + '\n' + processq, queryoutput);
                    view.querySelector('span[title="total row count"]').textContent = '≥' + results.length;
                    queryoutput.textContent = '';
                    queryoutput.appendChild(view);
                    lastdraw = now;
                }
            }
            if (x+1 < queue.length) {
                dactal.statusf('running queue: ' + (x + 1) + '/' + queue.length + ' -> ' + results.length);
                setTimeout(async () => {await loopf(x+1, torecache)}, 0);
            } else {
                dactal.timelimit = oldlimit;
                dactal.recache = false;
                if (postq) {
                    postresults = await dactal.query(postq, results);
                    resolve(postresults);
                } else {
                    resolve(results);
                }
            }
        };
        await loopf(0, oldrecache);
    });
}

const firstkeys = []; // was originally ['id', 'name'];
const lastkeys = ['key', 'of'];
function getkeys(obj) {
    if (obj == null || obj == undefined | typeof obj !== 'object') return [];
    return firstkeys.filter((k) => k in obj).concat(Object.keys(obj).filter((k) => !firstkeys.includes(k) && !lastkeys.includes(k))).concat(lastkeys.filter((k) => k in obj));
}

function escapequery(query) {
    return (query || '').replaceAll("'", '&squo;').replaceAll('"', '&quot;').replaceAll("\n", "<br>");
}

function unescapequery(queryescaped) {
    return queryescaped.replaceAll("<br>", "\n").replaceAll('&quot;', '"').replaceAll('&squo;', "'");
}

var cellrowstartmax = 10;
const renderers = {};

function add_renderer(fields, renderer) {
    fields.forEach((field) => (renderers[field] ??= []).push(renderer));
}

const filetypeloaders = {};
function add_loader(filetype, loaderf) {
    filetypeloaders[filetype] = loaderf;
}

const audiolimit = 100;
var audioshown = 0;
function render(obj, basequery, field, index, within=null) {
    let objquery;
    if (obj == null || obj == undefined) return '';
    if (field in renderers) {
        for (renderer of renderers[field]) {
            if (renderer.data) {
                rendered = renderer.data(obj, basequery, field, index);
                if (rendered != null) return rendered;
            }
        }
    }
    if (basequery && basequery != '') {
        objquery = basequery + ':@' + index + '.' + dactal.bracket(field); 
    } else {
        basequery = '';
        objquery = '';
    }
    if (field == 'query' && typeof obj == 'string') {
        return '<span class=querystring onclick="runQuery(unescapequery(\'' + escapequery(obj) + '\'))">' + h(obj) + '</span>';
    } else if (lastkeys.includes(field) && objquery.length > 0) {
        if (obj.length == 1 && (ofval = dactal.getname(obj[0]))) {} else if(obj.length === 0) {ofval = ''} else ofval = '...' + obj.length;
        return '<span class="fade' + (ofval.toString().startsWith('...') ? ' elided' : '') + '" onclick="hide=[];runQuery(unescapequery(\'' + escapequery(objquery) + '\'))">' + field + (obj ? ' [' + ofval + ']' : '') + '</span>'
    } else if (Array.isArray(obj)) {
        if (obj.length === 0) {
            return '&nbsp;';
        } else if (field == 'results' && basequery.startsWith('queries')) {
            return '<span class=fade>' + obj.length + '</span>';
        } else if (obj[0] && typeof obj[0] == 'object' && (!index || basequery == '' || Object.keys(obj[0]).filter((k) => !firstkeys.includes(k) && !lastkeys.includes(k)).length > 0 || (Object.keys(obj[0]).length == 1 && 'of' in obj[0]))) {
            if (countNodes(obj.slice(0)) == null) {
                return '<span class=fade>' + obj.slice(0).length + ' not shown</span>';
            } else if (Object.keys(obj[0]).length == 1 && obj[0].images) {
                return obj.map((strips) => render(strips)).join('<br>');
            } else {
                const table = arrayToTable(obj.slice(0), objquery, within, false);
                return table.outerHTML;
            }
        } else {
            if (obj.length == 1 || !index) {
                return render(obj[0], objquery, field, 1, within);
            } else {
                return '<span class="cellcount fade" onclick="tcl=this.nextSibling.classList; ex=\'expanded\'; if (tcl.contains(ex)) {tcl.remove(ex)} else {tcl.add(ex)}">' + obj.length + '</span><div class=' + (obj.length > cellrowstartmax ? 'longlist' : 'shortlist') + '><div class="expanddots otherlink" onclick="this.parentElement.classList.add(\'expanded\')">...</div><div>' + obj.map((o, oi) => render(o, objquery + ':@' + index, null, oi + 1, within)).join('<br>') + '</div></div>';
            }
        }
    } else if (typeof obj == 'object' && obj != null && obj != undefined) {
        if (obj.name !== null && obj.name !== undefined && obj.uri && Object.keys(obj).filter((key) => key != 'id').length == 2) {
            return '<a href="' + obj.uri + '">' + h(obj.name) + '</a>'
        } else if (Object.keys(obj).length == 1 && obj.images) {
            return (Array.isArray(obj.images) ? obj.images : [obj.images]).map((x) => `<img src="${x.image || x.url}" height="${x.height || ''}px" width="${x.width || ''}px" style="max-height: ${x.height || ''}px">`).join('');
        } else if (Object.keys(obj).length == 1 && obj.name != null && obj.name != undefined) {
            return h(obj.name);
        } else if (!index || !basequery || basequery == '') {
            return arrayToTable([obj], objquery, within, false).outerHTML;
        } else {
            const proptable = document.createElement('table');
            Object.keys(obj).forEach((k) => {
                const proprow = makeElement('tr', proptable, '', ['proptablerow']);
                if (!within) {
                    makeElement('td', proprow, '<span onclick="runQuery(unescapequery(\'' + escapequery(objquery) + '.' + dactal.bracket(k) + '\'))">' + h(k) + '</span>', ['queryheader']);
                } else {
                    makeElement('td', proprow, h(k), ['queryheader']);
                }
                makeElement('td', proprow, render(obj[k], objquery, k, null, within));
            })
            return proptable.outerHTML;
        }
    } else if (field == 'id' && (typeof obj == 'string' || typeof obj == 'number') && basequery?.length > 0) {
        return '<span onclick="runQuery(unescapequery(\'' + escapequery(basequery + ':' + dactal.bracket(obj)) + '\'))">' + h(obj) + '</span>';
    } else if (typeof obj == 'function') {
        return '<pre>' + h(obj) + '</pre>';
    } else if (typeof obj == 'string') {
        if (obj.startsWith('https:') || obj.startsWith('http:')) {
            if (obj.includes('image') || obj.includes('mosaic') || obj.endsWith('.png') || obj.endsWith('.jpg') || obj.endsWith('.jpeg')) {
                return '<a href="' + obj + '"><img src\="' + obj + '" target=image></a>'
            } else if (obj.includes('mp3') || obj.endsWith('m4a')) {
                if (audioshown < audiolimit) {
                    audioshown += 1;
                    return '<audio controls src="' + obj.replace(/\?.*/, '') + '">';
                } else {
                    return '<span class=fade>[too many audio players]</span>';
                }
            } else if (obj.includes('?')) {
                return '<a href="' + obj + '" target=link>' + h(obj.split('?')[0]) + '<span class=fade>?...</span></a>';
            } else {
                return '<a href="' + obj + '" target=link>' + h(obj) + '</a>';
            }
        } else if (obj.match(/^<img src="(.*?)" height=\d+px width=\d+px>$/)) {
            return obj;
        } else if (field == 'uri' || field?.endsWith('_uri')) {
            return '<a href="' + h(obj) + '" target=uri>' + h(obj) + '</a>';
        } else if (obj.startsWith('<pre>') && obj.endsWith('</pre>')) {
            return '<pre>' + h(obj.substr(5, obj.length - 11)) + '</pre>';
        } else if (obj.startsWith('<a href=') && obj.endsWith('</a>')) {
            return obj;
        } else if ((obj.trim().startsWith('<') && obj.trim().endsWith('>') && obj.length >= 20) || obj.match(/<br>/)) {
            const doc = new DOMParser().parseFromString(obj.trim(), "text/html");
            const style = doc.createElement("style");
            style.textContent = document.querySelector('style').textContent;
            (doc.head || doc.documentElement.insertBefore(doc.createElement("head"), doc.body)).appendChild(style);
            
            const srcdoc = "<!doctype html>\n" + doc.documentElement.outerHTML;
            return `<iframe class="fieldframe" sandbox="allow-same-origin" srcdoc="${escapeAttr(srcdoc)}" width=600px></iframe>`;
        } else if (obj.match(/<a href=/)) {
            return h(obj).replaceAll(/&lt;a href=&quot;(.*?)&quot;(.*?)&gt;/g, '<a href="$1"$2>').replaceAll(/&lt;\/a&gt;/g, "</a>");
        } else if (obj.match(/^ +$/)) {
            return '&nbsp;'
        } else  if (obj.match(/https?:/)) {
            return h(obj).replaceAll(/https?:\/\/[^\s<]+[^\s<.,;:!?)\]}]/g, url => `<a href="${url}">${url}</a>`);
        } else {
            return h(obj);
        }
    } else {
        if (typeof obj == 'number') {
            if (Number.isInteger(obj) || Math.abs(obj) < .00001) {
                objstr = obj.toString();
            } else if (Math.abs(obj) >= 10) {
                objstr = obj.toFixed(1);
            } else if (Math.abs(obj) >= .001) {
                objstr = obj.toFixed(3);
            } else {
                objstr = obj.toFixed(5);
            }
        } else {
            objstr = obj.toString();
        }
        return h(objstr);
    }
}

function heatmap(data, basequery) {
    const index = {}
    const xvals = data[0].xvals;
    const yvals = data[0].yvals;
    const vvals = data[0].vvals.filter((v) => v && isFinite(v));
    const svals = (data[0].svals || []).filter((v) => v && isFinite(v));
    const axes = data[0].axes;
    cells = data[0].of;
    cells.forEach((item) => {
        if (isFinite(item.v)) {
            if (typeof item.x != 'string') item.x = (item.x ?? '').toString();
            if (typeof item.y != 'string') item.y = (item.y ?? '').toString();
            if (item.y && !(item.y in index)) {
                index[item.y] = {};
            } 
            if (item.x && item.y && !(item.x in index[item.y])) {
                index[item.y][item.x] = {v: item.v, s: item.s};
                Object.keys(item).filter((k) => !['x', 'y', 'v', 'x', 'of'].includes[k] && dactal.dtype(item[k], 'literal')).forEach((k) => index[item.y][item.x][k] = item[k]);
            }
        }
    });
    const vmax = Math.max(...vvals);
    const vminraw = Math.min(...vvals);
    const vmin = vminraw == vmax ? 0 : vminraw;
    const smax = Math.max(...svals);
    const sminraw = Math.min(...svals);
    const smin = sminraw == smax ? 0 : sminraw;
    const heatmapbox = makeElement('div', null, '', ['heatmapbox']);
    if (axes?.length >= 2) {
        const axislabels = ['&darr; ' + h(axes[0]), '&rarr; ' + h(axes[1])];
        if (axes.length >= 3) axislabels.push('# ' + h(axes[2]));
        if (axes.length >= 4) axislabels.push('<span style="background: firebrick; padding: 0px 3px; font-size: 80%">&nbsp;</span> ' + h(axes[3]));
        makeElement('div', heatmapbox,  axislabels.join(' &nbsp; '), ['heatmaplegend']);
    }
    const heatmaptable = makeElement('table', heatmapbox, '', ['heatmap']);
    (xvals[0] || '').toString().split('\\').forEach((xpart, xpartx) => {
        const heatmapheaderrow = makeElement('tr', heatmaptable, '', ['heatmapheaderrow']);
        (yvals[0] || '').toString().split('\\').forEach(() => makeElement('td', heatmapheaderrow, '', ['heatmapheader']));
        Array.from(xvals, (x) => {
            const xcell = makeElement('td', heatmapheaderrow, '<span onclick="runQuery(this.parentElement.xquery)">' + h((x == null ? '' : x).toString().split('\\')[xpartx]) + '</span>', ['heatmapheader']);
            xcell.xquery = escapequery(basequery) + '..of:x=' + dactal.bracket(x) + '..of;_';
        });
    });
    Array.from(yvals, (y) => {
        const yrow = makeElement('tr', heatmaptable, '', ['heatmaprow']);
        (y == null ? '' : y).toString().split('\\').forEach((ypart) => {
            const ycell = makeElement('td', yrow, '<span onclick="runQuery(this.parentElement.yquery)">' + h(ypart) + '</span>', ['heatmaprowtitle']);
            ycell.yquery = escapequery(basequery) + '..of:y=' + dactal.bracket(y) + '..of;_';
        });
        Array.from(xvals, (x) => {
            const xcell = makeElement('td', yrow, '&nbsp;', ['heatmapcell']);
            if (y in index && index[y] && x in index[y] && (vs = index[y][x])) {
                const {v, s, ...otherkeys} = vs;
                let vval = v;
                if (Array.isArray(vval)) vval = vval[0];
                if (typeof vval == 'number' && !Number.isInteger(vval)) vval = vval.toFixed(1);
                xcell.xquery = escapequery(basequery) + '..of:x=' + dactal.bracket(x) + ':y=' + dactal.bracket(y) + '..of;_';
                xcell.innerHTML = '<span class=overridden onclick="runQuery(this.parentElement.xquery)">' + h(vval) + '</span>';
                xcell.style.backgroundColor = hsl(v, vmin, vmax, s, smin, smax);
                Object.entries(otherkeys).forEach(([k, ov]) => xcell.setAttribute(k.replaceAll(/\W/g, ''), ov));
                let vrev = vmax - (v - vmin);
                let vmindiff = .25 * (vmax - vmin);
                if (Math.abs(v - vrev) < vmindiff) {
                    if (vrev < v) {vrev = v - vmindiff} else {vrev = v + vmindiff};
                }
                let srev = null;
                if (!isNaN(s)) {
                    srev = smax - (s - vmin);
                    let smindiff = .1 * (smax - smin);
                    if (Math.abs(s - srev) < smindiff) {
                        if (srev < s) {srev = s - smindiff} else {srev = s + smindiff};
                    }
                }
                xcell.style.color = hsl(vrev, vmin, vmax, srev, smin, smax);
            }
        });
    });
    return heatmapbox;
}

function hsl(v, vmin, vmax, s=null, smin=null, smax=null) {
    if (s && smin && smax) {
        sval = Math.round(100 - 100 * (s - smin) / smax) + '%';
    } else {
        sval = '0';
    }
    return 'hsl(0 ' + sval + ' ' + Math.round(100 - 100 * (v - vmin) / vmax) + '%)'
}

async function loadquery(queryname) {
    const q = dactal.data.queries.find((q) => q.name == unescapequery(queryname));
    hide = q.hide || q.hidden || [];
    runQuery(q.query, q.name, q.results);
    window.scrollTo(0, 0);
    let url = new URL(window.location);
    if (url.searchParams.has('query')) {
        url.searchParams.set('query', queryname);
        url.searchParams.delete('queryname');
        history.replaceState(null, null, url);
    }
}

async function yield() {return new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve)))}

async function runorder(tag=null) {
    const oldlimit = dactal.timelimit;
    dactal.timelimit = 60 * 60 * 1000;
    let queue = dactal.data.queries.filter((q) => q.tag === tag && q.order).sort((a, b) => a.order - b.order);

    status('running ' + queue.length + (queue.length == 1 ? ' query' : ' queries'));
    await yield();

    async function runandsave(queue) {
        q = queue.shift();
        q.results = await dactal.query(q.query);
        status(q.order + '  ' + q.name + ' - found ' + q.results.length);
        await yield();
        if (queue.length > 0) {
            window.setTimeout(() => runandsave(queue), 0);
        } else {
            status('saving queries');
            await yield();
            await dsave('queries');
            status();
            byid('savedqueries').innerHTML = await loadqueries();
            dactal.timelimit = oldlimit;
        }
    }
    await runandsave(queue);
}

async function loadtypes() {
    if (Object.keys(dactal.data).length > 0 && (typestarts = document.getElementById('typestarts'))) {
        typestarts.innerHTML = typelinks();
    }
}

function querypreset(q) {
    dactal.data.queries ||= [];
    const qexist = dactal.data.queries.find((dq) => dq.name == q.name);
    if (!qexist) {
        q.results ??= [];
        dactal.data.queries.push(q);
        dactal.savedquerynames.add(q.name);
        dsave('queries');
    }
}

function typelinks() {
    const activedatasets = Object.keys(dactal.data).sort((a, b) =>
        (dactal.internal_datasets.includes(a) ? 1 : 0) - (dactal.internal_datasets.includes(b) ? 1: 0) ||
        (a.endsWith(' help') ? 1 : 0) - (b.endsWith(' help') ? 1 : 0) ||
        a.toLowerCase().localeCompare(b.toLowerCase())
    );
    return activedatasets.map((k) => '<span class=typewrapper typename="' + escapequery(k) + '"><span class="querytype spanlink otherlink' + (dactal.internal_datasets.includes(k) ? ' metadataset' : '') + '">' + h(k) + '</span><span class="deletetype otherlink" title="delete this data">&times;</span><span class="exporttype otherlink" title="export this data">&#x21A1;</span></span>').join('<wbr>');
}

async function hidecolumn(field) {
    if (!hide.includes(field)) hide.push(field);
    res = arrayToTable(query_results, dactal.data['query history'][0].query);
    queryoutput = document.getElementById('queryoutput');
    queryoutput.querySelector('.hiddenfields').innerHTML = hiddenfields();
    queryoutput.querySelector('table').replaceWith(res);
    queryoutput.querySelector('#dactalsavebutton').disabled = false;
}

async function unhidecolumn(field) {
    if (hide.includes(field)) hide = hide.filter((h) => h != field);
    res = arrayToTable(query_results, dactal.data['query history'][0].query);
    queryoutput = document.getElementById('queryoutput');
    queryoutput.querySelector('.hiddenfields').innerHTML = hiddenfields();
    queryoutput.querySelector('table').replaceWith(res);
    queryoutput.querySelector('#dactalsavebutton').disabled = false;
}

async function filtervals(field, basequery, currentresults, e) {
    const fvals = await dactal.execute(currentresults, dactal.parse('.' + dactal.bracket(field) + '#'));
    const flist = makeElement('div', document.body, '', ['filterlist']);
    flist.setAttribute('popover', ''); 
    for (fobj of fvals) {
        const fval = dactal.getname(fobj);
        const f = makeElement('div', flist, fval, ['filterval']);
        f.setAttribute('field', field);
        f.setAttribute('fval', fval);
        f.setAttribute('onclick', '');
        f.addEventListener('click', (e) => {
            e.stopPropagation();
            const me = e.currentTarget;
            let filterquery = ':' + dactal.bracket(me.getAttribute('field')) + '=' + dactal.bracket(me.getAttribute('fval'));
            runQuery(basequery + (e.metaKey ? ':-(' : '') + filterquery + (e.metaKey ? ')' : ''));
            me.parentElement.hidePopover();
        });
    }
    const f = makeElement('div', flist, '<span class=note>any value</span>', ['filterval']);
    f.setAttribute('field', field);
    f.setAttribute('onclick', '');
    f.addEventListener('click', (e) => {
        e.stopPropagation();
        const me = e.currentTarget;
        runQuery(basequery + ':' + dactal.bracket(me.getAttribute('field')) + (e.metaKey ? '-' : '+'));
        me.parentElement.hidePopover();
    });
    const rect = e.target.getBoundingClientRect();
    flist.style.position = 'absolute';
    flist.style.top  = `${rect.top  + window.scrollY}px`;
    flist.style.left = `${rect.left + window.scrollX}px`;
    flist.showPopover();
}


async function afterquery(query_results, query) {
    // called after each query
    // no-op by default, but can be overridden by apps
    // if a DOM node is returned, it will be inserted into the afterquery box
    // if text is returned, it will be assigned to afterquery.innerHTML
}




// Data import/export

async function dsave(dataset, conditional=false) {
    if (!conditional || dactaldb.persistent_query_history) dactaldb.set(dataset, dactal.data[dataset])
}

async function import_data(exported_data, overwrite=false) {
    const toload = [];
    Object.entries(exported_data.data)
        .filter(([k, v]) => k == 'connectors' || !dactal.internal_datasets.includes(k))
        .forEach(([k, v]) => {
            if (!(k in dactal.data) || overwrite) {
                console.log({overwriting: k})
                if (typeof v == 'string') {
                    toload.push([k, v])
                } else {
                    dactal.load(v, k);
                    dsave(k);
                }
            }
        });
    await toload.forEach(async ([k, filename]) => {
        const vres = await fetch('data/' + filename);
        const vdata = await vres.json();
        dactal.data[k] = Array.isArray(vdata) ? vdata : Object.values(vdata.data)[0];
        dsave(k);
    });
    console.log({savedquerynames: dactal.savedquerynames});
    let queriesadded = 0;
    await exported_data.queries?.filter((q) => !dactal.savedquerynames.has(q.name) || overwrite).forEach((q) => {
        // console.log({import_query: q.name});
        if (dactal.data.queries) {
            dactal.data.queries = dactal.data.queries.filter((oldq) => oldq.name != q.name);
        } else {
            dactal.data.queries = [];
        }
        dactal.data.queries.push(q);
        dactal.savedquerynames.add(q.name);
        queriesadded += 1;
    });
    if (queriesadded > 0) dsave('queries');
    console.log({queries: dactal.data.queries, savedqueries: dactal.savedquerynames});
    let indexesadded = 0;
    if (exported_data.index) {
        Object.entries(exported_data.index).forEach(([k, kv]) => {
            if (!(k in dactal.index)) dactal.index[k] = {};
            Object.entries(kv).forEach(([kk, kv]) => dactal.index[k][kk] = kv);
            indexesadded += 1;
        })
    }
    if (indexesadded > 0) dactaldb.set('_index', dactal.index);
    savedqueries = document.getElementById('savedqueries')
    if (savedqueries) savedqueries.innerHTML = await loadqueries();
}

async function import_from_url(url) {
    res = await fetch(url);
    data = await res.json();
    if (typeof data == 'object' && 'export' in data) import_data(data);
}

var filePrefixMap = {};
filePrefixMap.Streaming_History_Audio = 'streams';

async function loadFiles(inputwidget=null) {
    const fileInput = inputwidget || document.getElementById('fileInput') || document.getElementById('fileInput2');
    const files = fileInput.files;

    if (files.length === 0) {
        return;
    }

    const filePromises = Array.from(files, file => {
        return new Promise((resolve, reject) => {
            const reader = new FileReader();
            reader.onload = event => resolve({ name: file.name, content: event.target.result });
            reader.onerror = error => reject(error);
            if (file.name.endsWith('.car')) {
                reader.readAsArrayBuffer(file);
            } else {
                reader.readAsText(file);
            }
        });
    });

    Promise.all(filePromises)
    .then(filesContent => {
        filesContent.forEach(async (file) => {
            let target = file.name;
            let append = false;
            for ([prefix, target_dataset] of Object.entries(filePrefixMap)) {
                if (file.name.startsWith(prefix)) {
                    target = target_dataset;
                    append = true;
                }
            }
            if (file.name.endsWith('.json')) {
                filedata = JSON.parse(file.content);
                if (typeof filedata == 'object' && 'export' in filedata) {
                    await import_data(filedata);
                } else {
                    if (!append) target = file.name.replace(/\.json$/, '');
                    await dactal.load(filedata, target, append);
                    dsave(target);
                }
            } else if (file.name.endsWith('.jsonl')) {
                if (!append) target = file.name.replace(/\.jsonl$/, '');
                filedata = file.content;
                await dactal.loadjsonl(filedata, target, append);
                dsave(target);
            } else if (file.name.endsWith('.csv')) {
                if (!append) target = file.name.replace(/\.csv$/, '');
                await dactal.loadcsv(file.content, target);
                dsave(target);
            } else if (file.name.endsWith('.clf')) {
                if (!append) target = file.name.replace(/\.clf$/,'');
                await dactal.loadclf(file.content, target);
                dsave(target);
            } else if (file.name.endsWith('.rss')) {
                if (!append) target = file.name.replace(/\.rss$/, '');
                await dactal.loadrss(file.content, target);
                dsave(target);
            } else if (file.name.endsWith('.opml')) {
                if (!append) target = file.name.replace(/\.opml$/, '');
                await dactal.loadopml(file.content, target);
                dsave(target);
            } else {
                for (filetype in filetypeloaders) {
                    if (file.name.endsWith(filetype)) {
                        if (!append) target = file.name.slice(0, -(filetype.length));
                        await filetypeloaders[filetype](file.content, target);
                        dsave(target);
                        break;
                    }
                }
            }
        });
        fileInput.value = null;
        loadtypes();
    })
    .catch(error => console.error('Error loading files:', error));
}

var lastgap = 0;
function jumprow(e) {
    if (nextrow = afqsa('#queryresultstable > .datarow').find((r) => r.getBoundingClientRect().top > lastgap)) {
        const headerheight = nextrow.closest('table').querySelector('.queryheaderrow').clientHeight;
        let multiheight = 0;
        if (nextrow.previousSibling.classList.contains('queryheaderrow')) {
            const multibox = nextrow.closest('.dactalmultibox')?.querySelector('.dactalmultiheader');
            if (multibox) multiheight = multibox.clientHeight + 8;
        }
        lastgap = headerheight + multiheight;
        nextrow.style.scrollMarginTop = lastgap + 'px';
        nextrow.scrollIntoView({behavior: e.metaKey ? 'instant' : 'smooth', block: 'start', inline: 'start'});
    }
}

async function dactal_querymodule(queryname) {
    query = dactal.data.queries.find((q) => q.name == queryname);
    box = document.createElement('div');
    box.classList.add('dactalmodulebox');
    box.classList.add('queryhidden');
    box.id = queryname;
    box.basequery = query;
    box.hide = query.hide;
    makeElement('div', box, '<div class=querylabel><span class=querytitle>query</span><span class="hidequery fade fadelink" onclick="closest(\'.dactalmodulebox\').classList.toggle(\'queryhidden\')">hide</span></div><textarea class=queryta onkeydown="queryModuleKey(event)"></textarea><button class=querybutton onclick="queryModuleRun(event)" title="command-click to run without previously cached values">run</button><button class="querybutton queryresetbutton" onclick="queryModuleReset(event)" title="reset to the original query">reset</button><button class="querybutton explorebutton" onclick="queryModuleExplore(event)">explore</button>', ['queryinput']);
    box.querySelector('.queryta').value = query.query;
    afterquerybox = makeElement('div', box, '', ['afterquery']);
    queryoutput = makeElement('div', box, '', ['queryresults']);
    hqheader = makeElement('div', queryoutput, '', ['hiddenqueryheader']);
    makeElement('span', hqheader, 'show query', ['showquery', 'fade', 'fadelink']).addEventListener('click', (e) => e.target.closest('.dactalmodulebox').classList.toggle('queryhidden'));
    viewbox = makeElement('div', queryoutput, '', ['dactalviewbox']);
    hide = query.hide;
    res = buildView(query.results, query.query);
    viewbox.appendChild(res);
    return box;
}

async function queryModuleRun(e) {
    const qm = e.target.closest('.dactalmodulebox');
    const currentquery = qm.querySelector('.queryta').value;
    const viewbox = qm.querySelector('.dactalviewbox');
    hide = qm.hide;
    const results = await dactal.query(currentquery);
    const res = buildView(results, currentquery);
    viewbox.textContent = '';
    viewbox.appendChild(res);
}

async function queryModuleKey(e) {
    const qm = e.target.closest('.dactalmodulebox');
    if (e.key == 'Enter' && !e.shiftKey) {
        setTimeout(() => {
            e.target.style.background = '#F0F0F0';
            document.body.style.cursor = 'progress';
        }, 0);
        e.preventDefault();
        await queryModuleRun(e);
        setTimeout(() => {
            e.target.style.background = '';
            document.body.style.cursor = '';
        }, 1);
    } else if (e.key == 'Tab') {
        e.preventDefault();
        const ta = qm.querySelector('.queryta');
        const start = ta.selectionStart;
        const end = ta.selectionEnd;
        ta.value = ta.value.substring(0, start) + '\t' + ta.value.substring(end);
        ta.selectionStart = ta.selectionEnd = start + 1;
    }
}

async function queryModuleReset(e) {
    const qm = e.target.closest('.dactalmodulebox');
    qm.querySelector('.queryta').value = qm.basequery.query;
    await queryModuleRun(e);
}

async function queryModuleExplore(e) {
    const qm = e.target.closest('.dactalmodulebox');
    const query = qm.basequery;
    hide = query.hide;
    const currentquery = qm.querySelector('.queryta').value;
    await switch_to_querypage();
    await runQuery(currentquery, query.name);
}

async function dactal_queryform() {
    box = document.createElement('div');
    box.id = 'dactalouterbox';
    if (!qs('style.dactal_css')) css = makeElement('style', document.head, dactal_css);
    makeElement('div', box, '<div class=querylabel><span class="querylink fade" onclick="queryReset()">query</span><span class="hidequery fade fadelink" onclick="document.body.classList.toggle(\'queryhidden\')">hide</span></div><textarea id=dactal onkeydown="queryKey(event)"></textarea><button class=querybutton onclick="queryButton(event.metaKey || event.altKey)" title="command-click to run without previously cached values">run</button>', ['queryinput']);
    afterquerybox = makeElement('div', box, '', ['afterquery']);
    afterquerybox.id = 'afterquerybox';
    
    querystarts = makeElement('div', box, '', ['querystarts']);
    typestarts = makeElement('div', querystarts, typelinks(), ['typestarts']);
    typestarts.id = 'typestarts';
    typestarts.addEventListener('click', async (e) => {
        if (typename = unescapequery(event.target.closest('.typewrapper')?.getAttribute('typename'))) {
            if (e.target.classList.contains('querytype')) {
                if (dactal.internal_datasets.includes(typename)) {
                    await runQuery(dactal.bracket(typename));
                } else {
                    await runQuery(dactal.bracket(typename) + '???sample:@<=10');
                }
            } else if (e.target.classList.contains('deletetype')) {
                await deletetype(typename);
            } else if (e.target.classList.contains('exporttype')) {
                await export_data(typename, [], e.metaKey);
            } 
        }
    });
    
    savedqueries = makeElement('div', querystarts, '', ['savedqueries']);
    savedqueries.id = 'savedqueries';
    savedqueries.innerHTML = await loadqueries();
    
    adapters = makeElement('div', querystarts, '', ['adapters']);
    adapters.id = 'adapters';
    show_adapters();
    
    adddata = makeElement('div', querystarts, '<input type="file" id="fileInput" multiple onchange="loadFiles()"><button id=loadbutton onclick="document.getElementById(\'fileInput\').click()">Load more data</button>', ['adddata']);
    
    makeElement('div', querystarts, dactalintro);
    queryoutput = makeElement('div', box, '', ['queryresults', 'queryoutput']);
    queryoutput.id = 'queryoutput';
    return box;
}

function show_adapters() {
    adapterkeys = Object.keys(dactal.adapters);
    if (adapterkeys.length > 0 && (adapterdiv = byid('adapters'))) {
        adapterdiv.innerHTML = 'adapters: ' + adapterkeys.map((adapter) => h(adapter)).join(', ') + '<div class=fade>retry: <span class="fade fadelink" onclick="dactal.recache=new Set();">next query</span>, <span class="fade fadelink" onclick="retryAdapterMisses(event)">all misses</span></div>';
    }
}

var dactal_data_initialized = false;
async function dactal_data_init(dparams=null) {
    const start = performance.now();
    if (dactal_data_initialized || !dactaldb) return;
    const keys = await dactaldb.keys();
    if (!keys.includes('_index') && !dparams?.['noindex']) keys.push('_index');
    for (const k of keys) {
        if (k == '_index') {
            dactal.statusf('loading index');
            const indexdata = await dactaldb.get('_index');
            Object.assign(dactal.index, indexdata);
        } else if (!k.startsWith('_') && (!(k in dactal.data) || k == 'queries' || k == 'query history')) {
            dactal.statusf('loading ' + k);
            kdata = await dactaldb.get(k);
            if (Array.isArray(kdata)) {
                if (!(k in dactal.data)) {
                    dactal.data[k] = kdata;
                    if (k == 'queries') dactal.data.queries.forEach((q) => dactal.savedquerynames.add(q.name));
                } else if (k == 'queries') {
                    kdata.forEach((kq) => {
                        const foundq = dactal.data.queries.find((q) => q.name == kq.name); 
                        if (!foundq) {
                            dactal.data.queries.push(kq);
                            dactal.savedquerynames.add(kq.name);
                        } else if (!foundq.results?.length > 0) {
                            foundq.results = kq.results;
                        }
                    });
                } else if (k == 'query history') {
                    kdata.forEach((kq) => {
                        if (!dactal.data['query history'].includes(kq)) dactal.data['query history'].push(kq);
                    })
                }
            }
        }
    }
    dactal_data_initialized = true;
    const end = performance.now();
    console.log(`dactal data init: ${((end - start) / 1000).toFixed(1)}s`);
    dactal.statusf();
}

var switch_to_querypage = dactal_querypage;
async function dactal_querypage() {
    await dactal_data_init();
    box = document.createElement('div');
    box.id = 'dactalouterbox';
    if (!qs('style.dactal_css')) css = makeElement('style', document.head, dactal_css);
    makeElement('div', box, '<span class="querylink fade" onclick="queryReset()">query</span> <textarea id=dactal onkeydown="queryKey(event)"></textarea><button class=querybutton onclick="queryButton(event.metaKey || event.altKey)" title="command-click to run without previously cached values">run</button><button class=debugbutton onclick="dactal.debug=true;queryButton(event.metaKey || event.altKey);dactal.debug=false">debug</button>', ['queryinput']);
    afterquerybox = makeElement('div', box, '', ['afterquery']);
    afterquerybox.id = 'afterquerybox';

    querystarts = makeElement('div', box, '', ['querystarts']);
    typestarts = makeElement('div', querystarts, typelinks(), ['typestarts']);
    typestarts.id = 'typestarts';
    typestarts.addEventListener('click', async (e) => {
        const typenameraw = event.target.closest('.typewrapper')?.getAttribute('typename');
        if (typenameraw) {
            const typename = unescapequery(typenameraw);
            if (e.target.classList.contains('querytype')) {
                if (dactal.internal_datasets.includes(typename)) {
                    await runQuery(dactal.bracket(typename));
                } else {
                    await runQuery(dactal.bracket(typename) + '???sample:@<=10');
                }
            } else if (e.target.classList.contains('deletetype')) {
                await deletetype(typename);
            } else if (e.target.classList.contains('exporttype')) {
                await export_data(typename, [], e.metaKey);
            }
        }
    });

    savedqueries = makeElement('div', querystarts, '', ['savedqueries']);
    savedqueries.id = 'savedqueries';
    savedqueries.innerHTML = await loadqueries();
    
    adapters = makeElement('div', querystarts, '', ['adapters']);
    adapters.id = 'adapters';
    adapterkeys = Object.keys(dactal.adapters);
    if (adapterkeys.length > 0) {
        adapters.innerHTML = 'adapters: ' + adapterkeys.map((adapter) => h(adapter)).join(', ') + '<div class=fade>retry: <span class="fade fadelink" onclick="dactal.recache=new Set();">next query</span>, <span class="fade fadelink" onclick="retryAdapterMisses(event)">all misses</span></div>';
    }
    
    adddata = makeElement('div', querystarts, '<input type="file" id="fileInput" multiple onchange="loadFiles()"><button id=loadbutton onclick="document.getElementById(\'fileInput\').click()">Load more data</button> <button id=exportallbutton onclick="export_data(dactaldb.dbname, [\'*\'], event.metaKey)">Export all ' + h(dactaldb.dbname) + ' datasets and queries</button>', ['adddata']);

    makeElement('div', querystarts, dactalintro);
    queryoutput = makeElement('div', box, '', ['queryresults', 'queryoutput']);
    queryoutput.id = 'queryoutput';
    
    document.addEventListener('keydown', function(e) {
        ae = document.activeElement;
        if (e.key.toLowerCase() === 'r' && !e.altKey && !e.metaKey && !e.ctrlKey && !['INPUT', 'SELECT', 'TEXTAREA'].includes(ae.tagName) && !ae.IsContentEditable) {
            jumprow(e);
        }
    });
    
    dactal.connect('fetch', async (ids) => {
        const res = [];
        for (id of ids) {
            raw = await fetch(id);
            console.log(raw);
            if (id.endsWith('.txt')) {
                datatext = await raw.text();
                data = datatext.split(/[\n\r]+/g).map((line) => line.trim()).filter((line) => line != '');
            } else {
                data = await raw.json();
            }
            if (data) {
                res.push({id: id, data: data})
            }
        }
        return res;
    });
    
    dactal.connect('load', async (ids) => {
        const res = [];
        for (id of ids) {
            status('loading ' + id);
            const raw = await fetch('/dactal_proxy.cgi?url=' + encodeURIComponent(id));
            try {
                const resj = await raw.json();
                if (resj) res.push({id: id, data: resj.results});
            } catch(e) {
                const raw = await fetch('/dactal_proxy.cgi?url=' + encodeURIComponent(id));
                const resraw = await raw.text();
                if (resraw) {
                    const datasetname = id.split('/').reverse()[0].split('.')[0];
                    if (id.includes('rss')) {
                        dactal.loadrss(resraw, datasetname);
                    } else {
                        dactal.loadcsv(resraw, datasetname);
                    }
                    if (dactal.data[datasetname]) {
                        res.push({id: id, data: dactal.data[datasetname]});
                    }
                }
            }
        }
        return res;
    });
    
    dactal.connect_annotator('load script', async (item) => {
        if (!item.script) return {loaded: false, error: 'no script specified'};
        try {
            if (item.namespace) {
                await loadscript_namespaced(item.script, item.namespace);
            } else {
                await loadscript(item.script);
            }
            dactal.data.connectors = (dactal.data.connectors || []).filter((c) => c.script != item.script).concat([item]);
            dsave('connectors');
            unindex('unload script', dactal.getid(item));
            return item;
        } catch (e) {
            if (item.namespace) {
                return {failed: item.script, namespace: item.namespace, error: e.message};
            } else {
                return {failed: item.script, error: e.message};
            }
        }
    }, ['script'], {requires: [
        {property: 'script', default: '(required)', description: 'URL of .js file'},
        {property: 'namespace', default: '(optional)', description: 'Namespace to prepend to adapter properties'}
    ], produces: 'Loads module'});
    
    dactal.connect_annotator('unload script', async (item) => {
        dactal.data.connectors = (dactal.data.connectors || []).filter((c) => c.script != item.script);
        dsave('connectors');
        unindex('load script', dactal.getid(item));
        return item;
    }, ['script'], {requires: [
        {property: 'script', default: '(required)', description: 'script to unload'}
    ], produces: 'Removes module from autoload list'});

    if (dactaldb.dbname == 'rss') {
        dactal.connect('load rss', async (ids) => {
            const res = [];
            for (id of ids) {
                status('loading ' + id);
                const rssres = await fetch('/dactal_proxy.cgi?url=' + encodeURIComponent(id));
                const raw = await rssres.text();
                const rss = await dactal.loadrss(raw, null);
                if (rss) res.push({id: id, posts: rss});
            }
            return res;
        });
        
        dactal.connect('load opml', async (ids) => {
            const res = [];
            for (id of ids) {
                const opmlres = await fetch('/dactal_proxy.cgi?url=' + encodeURIComponent(id));
                const raw = await opmlres.text();
                const opml = await dactal.loadopml(raw, null);
                if (opml) res.push({id: id, feeds: opml});
            }
            return res;
        });
    }
    
    add_renderer(['☑︎'], {
        header: (data, basequery, field) => ({text: `<span onclick="event.stopPropagation(); extendquery(unescapequery('${escapequery(basequery.replace(/\|☑︎@$/, ""))}'), null);">☑︎</span>`, opts: []}),
        data: (obj, basequery, field, index) => `<input type=checkbox class=usethese value="${obj}">`
    })
        
    return box;
}

function unindex(key, value) {
    delete(dactal.index[key][value]);
    dactaldb.set('_' + key, dactal.index[key]);
}

var dactalaftersave = (newquery) => {};
async function dactalsave(e=null) {
    querynameinput = document.getElementById('dactalsave');
    queryfield = document.getElementById('dactal');
    if (querynameinput && queryfield) {
        document.getElementById('dactalsavebutton').disabled = (querynameinput.value.length === 0);
        if (!e || e.target != querynameinput || e.key == 'Enter') {
            if (e) setTimeout(() => e.target.style.background = '#F0F0F0', 1);
            query = queryfield.value.trim();
            parsed = dactal.parse(query);
            relative = parsed?.length > 0 && parsed[0].operator != '?' && query_results?.length == 0;
            querynameinputval = querynameinput.value.trim();
            parts = querynameinputval.split(/ *: */);
            if (parts.length == 3) {
                [querytag, queryorder, queryname] = parts;
            } else if (parts.length == 2) {
                [querytag, queryname] = parts;
                queryorder = undefined;
            } else {
                queryname = querynameinputval;
                querytag = undefined;
                queryorder = undefined;
            }
            if (queryname.startsWith('.')) {
                queryname = queryname.slice(1);
                relative = true;
            }
            if (query && query.length > 0 && queryname && queryname.length > 0) {
                newquery = {name: queryname, query: query, results: query_results.slice(0), hide: hide.slice(0), time: query_time, relative: relative};
                if (querytag) newquery.tag = querytag;
                if (queryorder) newquery.order = Number(queryorder);
                existingquery = dactal.data.queries?.filter((q) => q.name == queryname);
                if (existingquery && existingquery.length == 1) {
                    const oldquery = existingquery[0];
                    for (qprop in oldquery) {
                        if (qprop == 'tag') {
                            if (querytag == undefined) newquery.tag = oldquery.tag;
                        } else if (qprop == 'order') {
                            if (queryorder == undefined) newquery.order = oldquery.order;
                        } else if ((qprop == 'relative' && oldquery.relative) || !(qprop in newquery)) {
                            newquery[qprop] = oldquery[qprop];
                        }
                    }
                }
                newqueries = [newquery].concat((dactal.data.queries || []).filter((q) => q.name != queryname))
                dactal.data.queries = newqueries;
                await dactaldb.set('queries', newqueries);
                dactal.savedquerynames.add(queryname);
                dactal.index[queryname] = {};
                if (sq = document.getElementById('savedqueries')) sq.innerHTML = await loadqueries();
                dactalaftersave(newquery);
            }
            if (e) setTimeout(() => e.target.style.background = '', 200);
        }
    }
}

async function loadqueries() {
    if (!dactal.data.queries || dactal.data.queries.length === 0) {
        dactal.data.queries = await dactaldb.get('queries');
    }
    if (dactal.data.queries && dactal.data.queries.length > 0) {
        sortedqueries = dactal.data.queries.sort((a, b) => 
            (a?.tag ? 1 : 0) - (b?.tag ? 1 : 0) ||
            a.tag?.localeCompare(b.tag) ||
            (a.order || Infinity) - (b.order || Infinity) ||
            (a.relative ? 1 : 0) - (b.relative ? 1 : 0) ||
            a.name.toLowerCase().localeCompare(b.name.toLowerCase()));
        lasttag = null;
        sortedqueries.forEach((q) => dactal.savedquerynames.add(q.name));
        if (sortedqueries[0].tag) {
            firstheading = '';
        } else {
            firstheading = '<div class=savedqueryheading>saved queries</div>';
        }
        return firstheading + sortedqueries.map(({name, query, tag, results, relative, order}) => {
            if (tag != lasttag) {
                if (dactal.data.queries.find((q) => q.tag == tag && q.order)) {
                    runall = '<span class="runorder fade" title="rerun this sequence" onclick="runorder(\'' + escapequery(tag) + '\')">↯</span>';
                } else {
                    runall = '';
                }
                tagheading = '<div class=savedquerytag>' + tag + '<span class="deletequery fade" title="delete all these queries and their results" onclick="deletetag(\'' + escapequery(tag) + '\')">&times;</span><span class="exportquery fade" title="export all these queries and their results" onclick="export_data(\'' + escapequery(tag) + '\', [], event.metaKey)">&#x21A1;</span>' + runall + '</div>';
                lasttag = tag;
            } else {
                tagheading = '';
            }
            if (results && results.length > 0) {
                resultstring = '<span class=queryresultcount>&rarr;' + results.length + '</span>'
            } else {
                resultstring = '';
            }
            orderstring = order ? '<span class=fade>' + h(order) + '</span> ' : '';
            if (query.length > 128) {
                querystring = '<span class="fade nolink queryshort" onclick="this.parentElement.classList.toggle(\'open\')">' + h(query.slice(0,100)) + '<span class=note>...</span></span><span class="fade nolink querylong" onclick="this.parentElement.classList.toggle(\'open\')">' + h(query) + '</span>'
            } else {
                querystring = '<span class="fade queryshort">' + h(query) + '</span>';
            }
            return tagheading + '<div class=savedquery>' + orderstring + '<span onclick="loadquery(\'' + escapequery(name) + '\')">' + (relative ? '<span class=fade title=relative>—.</span>' : '') + h(name) + '</span> &nbsp; ' + querystring + resultstring + '<span class="deletequery fade" onclick="deletequery(\'' + escapequery(name) + '\')" title="delete this query and its results">&times;</span><span class="exportquery fade" title="export this query and its results" onclick="export_data(\'' + escapequery(name) + '\', [], event.metaKey)">&#x21A1;</span></div>';
        }).join('');
    } else {
        return '';
    }
}

async function retryAdapterMisses(e) {
    setTimeout(() => e.target.style.fontWeight = 'bold', 1);
    const indexdata = await dactaldb.get('_index');
    dactal.index = indexdata;
    await dactal.rehope();
    await dactaldb.set('_index', indexdata); 
    setTimeout(() => e.target.style.fontWeight = '', 100);
}

async function deletequery(escapedname) {
    name = unescapequery(escapedname);
    if (window.confirm('Delete this query?')) {
        if (name && name.length > 0) {
            newqueries = dactal.data.queries.filter((q) => q.name != name);
            dactal.data.queries = newqueries;
            await dactaldb.set('queries', newqueries);
            dactal.savedquerynames.delete(name);
            dactal.index[name] = {};
            document.getElementById('savedqueries').innerHTML = await loadqueries();
        }
    }
}

async function deletetag(tag) {
    if (tag && tag.length > 0) {
        if (window.confirm('Delete all these queries?')) {
            dactal.data.queries.filter((q) => q.tag == tag).forEach((q) => {
                dactal.savedquerynames.delete(q.name);
                dactal.index[q.name] = {};
            });
            dactal.data.queries = dactal.data.queries.filter((q) => q.tag != tag)
            await dsave('queries');
            document.getElementById('savedqueries').innerHTML = await loadqueries();
        }
    }
}

async function deletetype(name) {
    if (window.confirm('Delete this whole dataset?')) {
        await dactaldb.remove(name);
        delete dactal.data[name];
        loadtypes();
    }
}

function sanitizeFilename(filename) {
  return filename.replace(/[^a-z0-9._-]/gi, '_').toLowerCase();
}

function escapeFilename(filename) {
  return filename.replace(/[^a-z0-9._-]/gi, '_');
}

async function export_data(exportname, tags=[], stripped=false) {
    const export_data = {export: exportname, data: {}};
    if (!stripped) {
        Object.entries(dactal.data)
            .filter(([k, v]) => k == 'connectors' || !dactal.internal_datasets.includes(k) || (k == exportname && k != 'queries'))
            .filter(([k, v]) => tags.includes('*') || tags.includes(k) || k == exportname).forEach(([k, v]) => export_data.data[k] = v);
    }
    dactal.data.queries.filter((q) => tags.includes('*') || exportname == 'queries' || tags.includes(q.tag) || q.tag == exportname || q.name == exportname).forEach((q) => {
        export_data.queries ??= [];
        if (stripped) {
            if (q.tag != 'private') {
                const {results, ...transfer} = q;
                export_data.queries.push(transfer);
            }
        } else {
            export_data.queries.push(q);
        }
    });
    // if (tags.length > 0 && !stripped) export_data.index = dactal.index;
    // console.log(export_data);
    Object.entries(export_data).forEach(([k, v]) => {
        console.log({outerk: k});
        console.table(Object.entries(v).map(([k, v]) => {
            console.log({innerk: k, innerv: v});
            return [k, JSON.stringify(v).length]
        }));
    })
    const datastr = "data:text/json;charset=utf-8," + encodeURIComponent(JSON.stringify(export_data));
    const a = document.createElement('a');
    a.href = datastr;
    a.download = escapeFilename(exportname) + '.json';
    document.body.appendChild(a);
    a.click();
    a.remove();
}

function editinline(path, value) {
    let current = dactal.data;
    for (let i = 0; i < path.length; i++) {
        const { field, id } = path[i];
        const list = current[field];
        if (!Array.isArray(list)) return false;
        const item = list.find((x) => x.id === id);
        if (!item) return false;
        if (i === path.length - 1) {
            Object.assign(item, value);
            console.log({item: item, assigned: value});
        } else {
            current = item;
        }
    }
    return true;
}

async function saveas(dname) {
    if (dactal.data['current results']?.length > 0 && !(dname in dactal.data)) {
        dactal.data[dname] = dactal.data['current results'].slice(0);
        await dsave(dname);
        loadtypes();
    }
}

function sendevent(eventname, target=document) {
    target.dispatchEvent(new CustomEvent(eventname, {bubbles: true}));
}