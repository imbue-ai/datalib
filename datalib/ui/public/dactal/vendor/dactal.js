class DACTAL {
    constructor(data={}) {
        this.data = data;
        this.index = {};
        this.index_modified = new Set;
        this.adapters = {};
        this.adaptive = false;
        this.features = { // earnest magic you can disable
            autoresolve: true, // if dataset X exists, following prop X to a literal is treated as an ID lookup
            plurality: true, // prop and props may be used interchangeably
            inlinemath: true, // literals starting with = do inline math, like .[=score/total]
            unscore: true, // props_like_this can also be referred to as props like this
            guessid: true, // items without ids may have them inferred from their names
            guessname: true // items without names may have them inferred from other properties
        }
        this.data['query history'] = [];
        this.savedquerynames = new Set();
        this.debug = false;
        this.recache = false;
        this.timelimit = 120000;
        this.statusf = (statusmsg) => {if (statusmsg) console.log(statusmsg)};
        this.internal_datasets = ['queries', 'query history', 'connectors', 'adapters', 'annotators', 'current results', 'data updates', 'assistance', '.clf API routes'];

        this.annotators = {
            group: (item) => item.of,
            label: (item) => item.of,
            count: (item) => this.vals(item).length,
            total: (item) => this.numvals(item).reduce((acc, val) => acc + val, 0),
            average: (item) => this.numvals(item).reduce((acc, val) => acc + val, 0) / this.numvals(item).length,
            median: (item) => {
                const nums = this.numvals(item).sort();
                return nums[Math.floor(nums.length/2)];
            },
            min: (item) => {
                const nums = this.numvals(item);
                return nums.length == 0 ? [] : Math.min(...nums)
            },
            max: (item) => {
                const nums = this.numvals(item);
                return nums.length == 0 ? [] : Math.max(...nums)
            },
            product: (item) => this.numvals(item).reduce((acc, val) => acc * val, 1),
            difference: (item) => this.numvals(item).reduce((acc, val) => acc - val),
            quotient: (item) => this.numvals(item).reduce((acc, val) => acc / val),
            percent: (item) => Math.round(this.numvals(item).reduce((acc, val) => acc / val) * 100),
            sqrt: (item) => Math.sqrt(this.numvals(item)[0]),
            log: (item) => Math.log(this.numvals(item)[0]),
            log10: (item) => Math.log10(this.numvals(item)[0]),
            abs: (item) => Math.abs(this.numvals(item)[0]),
            is: (item) => (item.of?.length > 0) ? 1 : 0,
            isnt: (item) => (item.of?.length === 0) ? 1 : 0,
            yesno: (item) => (item.of?.length > 0) ? 'yes' : 'no', 
            missing: (item) => (item.of?.length === 0) ? true : [],
            sortsame: (item) => {
                const sortforms = this.vals(item).map((v) => v?.toString()?.toLowerCase()?.replace(/^the /, ''));
                const sortformset = new Set(sortforms);
                return (sortformset.size == 1 ? 'sortsame' : []);
            },
            startsame: (item) => {
                const vals = this.vals(item);
                const shortest = vals.sort((a, b) => a.length - b.length)[0];
                return (vals.filter((v) => v.startsWith(shortest)).length == vals.length ? shortest : []);
            },
            concatenate: (item) => this.vals(item).join(' '),
            join: (item) => item.of.map(this.getname).join(this.vals(item)[0]),
            str: (item) => item.of.map(this.getname).join(''),
            'to json': (item) => JSON.stringify(item.of),
            quote: (item) => `“${this.vals(item)[0]}”`,
            url: (item) => {
                let u = item.of.map(this.getname).join('');
                if (!u.startsWith('https://')) u = 'https://' + u;
                for (const prop of this.kkeys(item)) {
                    const val = item[prop];
                    (Array.isArray(val) ? val : [val]).forEach((vv) => u = u + (u.match(/\?/) ? '&' : '?') + encodeURIComponent(prop) + '=' + encodeURIComponent(vv)); 
                }
                return u;
            },
            remove: (item) => this.vals(item).reduce((acc, val) => acc.replaceAll(val, '')),
            matches: (item) => {
                const matchers = item.match.map((m) => m.toLowerCase());
                return matchers.filter((m) => item.text.find((i) => this.getname(i).toLowerCase().includes(m)));
            },
            bmk: (item) => this.vals(item).map((v) => v.toString().toLowerCase().replaceAll(/[\$\xA2-\xA5\u058F\u060B\u09F2\u09F3\u09FB\u0AF1\u0BF9\u0E3F\u17DB\u20A0-\u20BD\uA838\uFDFC\uFE69\uFF04\uFFE0\uFFE1\uFFE5\uFFE6,+]/g, '').replaceAll('b', 'kkk').replaceAll('m', 'kk').replaceAll('k', '000')),
            sortform: (item) => this.vals(item).map((val) => val?.toString()?.toLowerCase()?.replace(/^the /, '')),
            zip: (item) => {
                const itemkeys = this.kkeys(item);
                const zipped = [];
                for (let i=0; i<item[itemkeys[0]].length; i++) {
                    const zipline = {};
                    for (const key of itemkeys) {
                        zipline[key] = item[key][i];
                    }
                    zipped.push(zipline);
                }
                return zipped;
            },
            pairs: (item) => {
                return item.of.slice(0, -1).map((val, vx) => ({pair: [val, item.of[vx+1]]}));
            },
            triples: (item) => {
                return item.of.slice(0, -2).map((val, vx) => ({triple: [val, item.of[vx+1], item.of[vx+2]]}));
            },
            quads: (item) => {
                return item.of.slice(0, -3).map((val, vx) => ({quad: [val, item.of[vx+1], item.of[vx+2], item.of[vx+3]]}));
            },
            sequences: (item) => {
                return item.of.map((val, valx, vallist) => ({sequence: vallist.slice(0, valx+1).map((val) => this.dcopy(val))}));
            },
            split: (item) => {
                const itemvals = this.vals(item);
                let splitter;
                let tobesplit;
                if (Object.keys(item).length == 1) itemvals.push(' ');
                if (itemvals.length == 1) {
                    splitter = itemvals[0];
                    tobesplit = item.of.slice(0);
                } else {
                    splitter = itemvals.pop();
                    tobesplit = itemvals.slice(0);
                }
                if (splitter.startsWith('~')) splitter = new RegExp(splitter.replace(/^~*/, ''), splitter.startsWith('~~') ? 'i' : '');
                return tobesplit.flatMap((v) => v.toString().split(splitter));
            },
            unpack: (item) => {
                return item.of.flatMap((val) => {
                    const valstr = val.toString();
                    return this.vals(item).reduce((acc, size, x) => {
                        acc.push(valstr.substring(0, size));
                        valstr = valstr.substring(size);
                        return acc;
                    }, []);
                });
            },
            extract: (item) => {
                const itemvals = this.vals(item);
                const delimiters = itemvals.pop();
                const res = [];
                for (const itemval of (itemvals.length > 0 ? itemvals : item.of)) {
                    for(let i=0; i<delimiters.length; i+=2) {
                        const d1 = delimiters[i];
                        const d2 = delimiters[i+1];
                        const d1x = itemval.indexOf(d1);
                        const d2x = itemval.indexOf(d2);
                        if (d1x > -1 && d2x > d1x) {
                            res.push(itemval.slice(d1x+1, d2x).trim());
                            break;
                        }
                    }
                }
                return res;
            },
            unchain: (item) => {
                const chainprops = this.kkeys(item);
                const unchained = [];
                const queue = item.of.slice(0);
                while (queue.length > 0) {
                    const thisitem = queue.shift();
                    if (!unchained.includes(thisitem)) {
                        unchained.push(thisitem);
                        for (const chainprop of chainprops) {
                            if (this.dtype(thisitem, 'object') && chainprop in thisitem) {
                                if (this.dtype(thisitem[chainprop], 'array')) {
                                    for (const x of thisitem[chainprop].slice(0).reverse()) queue.unshift(x);
                                } else if (thisitem[chainprop]) {
                                    queue.unshift(thisitem[chainprop])
                                }
                            }
                        }
                    }
                }
                return unchained;        
            },
            itemize: (item) => {
                const propname = item?.property ?? 'property';
                const valname = item?.value ?? 'value';
                return item.of.flatMap((subitem) => Object.entries(subitem).map(([key, val]) => ({[propname]: key, [valname]: val})));
            },
            schematize: (item) => {
                const itemkeys = this.kkeys(item);
                const schematized = {};
                item[itemkeys[0]].forEach((subitem) => {
                    let subkey;
                    let subval;
                    if (this.dtype(subitem, 'array')) {
                        const [subkey, subval] = subitem;
                    } else {
                        [subkey, subval] = Object.values(subitem);
                    }
                    schematized[subkey] = subval;
                });
                return schematized;
            },
            index: (item) => {
                return Object.entries(item).filter(([k, v]) => k != 'of').map(([k, v]) => ({id: k, name: !isNaN(v) ? Number(v) : v}));
            },
            unflatten: (item) => {
                const itemkeys = this.kkeys(item);
                if (itemkeys.length === 0) itemkeys.push('');
                const newindex = {};
                const neworder = [];
                item.of.forEach((subitem) => {
                    Object.keys(subitem).forEach((field) => {
                        itemkeys.forEach((key) => {
                            if (field.startsWith(key)) {
                                const subid = field.replace(key, '');
                                if (subid.length > 0) {
                                    if (!(subid in newindex)) {
                                        newindex[subid] = {};
                                        neworder.push(subid);
                                    }
                                    newindex[subid][key] = subitem[field];
                                }
                            }
                        });
                    });
                });
                itemkeys.forEach((key) => delete item[key]);
                return neworder.map((k) => {
                    const subitem = {};
                    subitem.subid = k;
                    Object.assign(subitem, newindex[k]);
                    return subitem;
                });
            },
            detupled: (item) => {
                const newobj = {};
                item.of.forEach((subitem) => {
                    if (Array.isArray(subitem) && subitem.length == 2) {
                        newobj[subitem[0]] = subitem[1];
                    }
                })
                return [newobj];
            },
            csv: (item) => {
                const keys = Object.entries(item).find(([k, v]) => k != 'of')[1];
                const res = [];
                const vals = item.of.flatMap((val) => this.dtype(val, 'string') ? val.split('\n').map((val) => val.trim()) : val);
                for (let i=0; i<vals.length; i+=keys.length) {
                    const newitem = {};
                    for (let k=0; k<keys.length; k++) {
                        newitem[keys[k]] = vals[i+k];
                    }
                    res.push(newitem);
                }
                return res;
            },
            tsv: (item) => {
                const text = this.getname(item);
                const lines = text.split('\n').filter((line) => line != '').map((line) => line.split('\t').map((val) => val.trim()));
                const keys = lines[0];
                return lines.slice(1).map((vals) => Object.fromEntries(vals.map((val, vi) => [keys[vi], val])));
            },
            ssv: (item) => {
                const text = this.getname(item);
                const lines = text.split('\n').filter((line) => line != '').map((line) => line.split(/ +/).map((val) => val.trim()));
                const keys = lines[0];
                return lines.slice(1).map((vals) => Object.fromEntries(vals.map((val, vi) => [keys[vi], val])));
            },
            json: (item) => this.vals(item).flatMap((v) => JSON.parse(v)),
            year: (item) => {
                if (item.date?.toString()?.length > 0) return item.date?.[0]?.toString()?.match(/\d\d\d\d/)?.[0];
                let yearmatch = this.getname(item.toString()).match(/\d\d\d\d/);
                if (yearmatch) {
                    return yearmatch[0];
                } else {
                    yearmatch = this.getid(item.of[0] || '').toString().match(/\d\d\d\d/);
                    if (yearmatch) {
                        return yearmatch[0];
                    }
                    return null;
                }
            },
            month: (item) => {
                if (item.date?.length > 0) return item.date[0].match(/\d\d\d\d-(\d\d)-\d\d/)[1];
                let monthmatch = this.getname(item).match(/\d\d\d\d-(\d\d)-\d\d/);
                if (monthmatch) {
                    return monthmatch[1];
                } else {
                    monthmatch = this.getid(item.of[0] || '').toString().match(/\d\d\d\d-(\d\d)-\d\d/);
                    if (monthmatch) {
                        return monthmatch[1];
                    }
                    return null;
                }
            },
            date: (item) => {
                let itemvals = this.vals(item);
                if (itemvals?.length > 0) {
                    let datematch = itemvals[0].toString().match(/\d\d\d\d-\d\d-\d\d/);
                    if (datematch) {
                        return datematch[0];
                    } else {
                        datematch = this.getid(item.of[0] || '').toString().match(/\d\d\d\d-\d\d-\d\d/);
                        if (datematch) {
                            return datematch[0];
                        } else {
                            if (this.dtype(itemvals[0], 'number')) {
                                datematch = itemvals[0].toString().slice(0, 4);
                                if (datematch) {
                                    return datematch;
                                }
                            }
                        }
                    }
                }
                return null;
            },
            weekday: (item) => {
                const days = ['sun', 'mon', 'tue', 'wed', 'thu', 'fri', 'sat', 'sun'];
                if (item.date?.length > 0) return days[new Date(item.date).getDay()];
            },
            time: (item) => this.vals(item).map((v) => v.split('T')[1].slice(0,5)),
            timeshift: (item) => {
                const vals = this.vals(item);
                let tsx = new Date(vals[0]);
                let adjust = Number(vals[1]) * 60*60*1000;
                tsx.setTime(tsx.getTime() + adjust);
                return tsx.toISOString();
            },
            hour: (item) => this.vals(item).map((v) => v.split('T')[1].split(':')[0]),
            datediff: (item) => {
                const [d1, d2] = this.vals(item);
                return ((d2 ? new Date(d2) : new Date()) - new Date(d1)) / (24*60*60*1000);
            },
            timediff: (item) => {
                const [d1, d2] = this.vals(item);
                return (new Date(d2) - new Date(d1));
            },
            dateforms: (item) => {
                const basedate = this.vals(item)[0];
                const [baseyear, basemonth, baseday] = basedate.split('-');
                return [
                    basedate,
                    `${basemonth}/${baseday}/${baseyear}`,
                    basemonth.startsWith('0') ? `${basemonth.replace(/^0/,'')}/${baseday}/${baseyear}` : null,
                    basemonth.startsWith('0') && baseday.startsWith('0') ? `${basemonth.replace(/^0/,'')}/${baseday.replace(/^0/,'')}/${baseyear}` : null,
                    `${basemonth}/${baseday}/${baseyear.slice(2)}`,
                    `${basemonth.replace(/^0/,'')}/${baseday.replace(/^0/,'')}/${baseyear.slice(2)}`,
                    `${['', 'January', 'February', 'March', 'April', 'May', 'June', 'July', 'August', 'September', 'October', 'November', 'December'][Number(basemonth)]} ${baseday.replace(/^0/,'')}, ${baseyear}`
                ].filter(x => x);
            },
            now: (item) => performance.now(),
            spandays: (item) => {
                const startdate = Array.isArray(item.start) ? item.start[0] : item.start;
                const enddate = Array.isArray(item.end) ? item.end[0] : item.end;
                const days = [];
                const d = new Date(startdate);
                d.setUTCHours(0, 0, 0, 0);
                const stop = new Date(enddate);
                stop.setUTCHours(0, 0, 0, 0);
                
                while (d < stop) {
                    days.push(d.toISOString().slice(0, 10));
                    d.setUTCDate(d.getUTCDate() + 1);
                }
                return days;
            },
            round: (item) => this.numvals(item).map((v) => Math.round(v)),
            roundaway: (item) => this.numvals(item).map((v) => Math.sign(v) * Math.round(Math.abs(v))),
            roundm: (item) => {
                const vals = this.numvals(item);
                const multiple = vals.pop();
                return vals.map((v) => Math.round(v / multiple) * multiple);
            },
            roundd: (item) => {
                const vals = this.numvals(item);
                const digits = vals.pop();
                return vals.map((v) => {
                    const factor = 10 ** (Math.floor(Math.log10(v)) - digits + 1);
                    return Math.round(v / factor) * factor;
                });
            },
            floor: (item) => this.numvals(item).map((v) => Math.floor(v)),
            ceil: (item) => this.numvals(item).map((v) => Math.ceil(v)),
            n: (item) => {
                let i = 0;
                const test = this.getname(item.of[0]);
                for (const key in item) {
                    if (key != 'of') {
                        i--;
                        if (key == test) return i;
                    }
                }
                return i - 1;
            },
            numbers: (item) => {
                const res = [];
                const firstnum = item.from != undefined && item.from != null ? Number(item.from) : 1;
                const lastnum = item.to != undefined && item.to != null ? Number(item.to) : Number(this.getname(item.of[0]));
                for(let x=firstnum; x<=lastnum; x++) {
                    res.push(x)
                }
                return res;
            },
            dehyphenate: (item) => this.vals(item).flatMap((v) => v.replaceAll(/(?<=\w)-\n(?=\w)/g, '')),
            deparen: (item) => this.vals(item).flatMap((v) => v.replaceAll(/\s*\(.*?\)\s*/g, '')),
            sentences: (item) => this.vals(item).flatMap((v) => v.split(/[.?!…]['"’”»]?\s+/)),
            allwords: (item) => this.vals(item).flatMap((v) => Array.from(v.toString().toLowerCase().matchAll(/[\p{Letter}\p{Number}]+(?:['‘’][\p{Letter}\p{Number}]+)?/gu).map((m) => m[0]))),
            words: (item) => this.vals(item).flatMap((v) => Array.from(v.toString().toLowerCase().matchAll(/[\p{Letter}\p{Number}]+(?:['‘’][\p{Letter}\p{Number}]+)?/gu).map((m) => m[0])).filter((w) => w.length >= 4)),
            characters: (item) => this.vals(item).flatMap((v) => Array.from(v.toString())),
            'character count': (item) => this.vals(item).flatMap((v) => v.length),
            case: (item) => this.vals(item).map((v) => {
                const hasupper = v.match(/[A-Z]/);
                const haslower = v.match(/[a-z]/);
                if (!hasupper && !haslower) {
                    return 'none';
                } else if (hasupper && !haslower) {
                    return 'upper';
                } else if (haslower && !hasupper) {
                    return 'lower';
                } else {
                    if (v[0].match(/[A-Z]/) && !(v.slice(1).match(/[A-Z]/))) {
                        return 'initial';
                    } else {
                        return 'mixed';
                    }
                }
            }),
            uppercase: (item) => this.vals(item).flatMap((v) => v.toString().toUpperCase()),
            lowercase: (item) => this.vals(item).flatMap((v) => v.toString().toLowerCase().replaceAll(/[‘’`]/g, "'").replaceAll(/[“”]/g, '"')),
            list: (item) => this.kkeys(item),
            items: (item) => item?.of?.flatMap((typename) => this.data?.[this.getname(typename)]),
            traverse: (item) => {
                const props = this.vals(item);
                return item?.of?.flatMap((subitem) => props.flatMap((prop) => this.step(subitem, prop, null, {})));
            },
            random: (item) => Math.random(),
            shuffle: (item) => {
                const newArray = [].concat(item?.of || []);
                for (let i = newArray.length - 1; i > 0; i--) {
                    const j = Math.floor(Math.random() * (i + 1));
                    [newArray[i], newArray[j]] = [newArray[j], newArray[i]];
                }
                return newArray;
            },
            'weighted shuffle': (item) => {
                return item.of.map((subitem) => ({weight: Number(subitem.weight || 1) * Math.random(), subitem: subitem})).sort((a, b) => b.weight - a.weight).map((ws) => ws.subitem);
            },
            pick: (item) => item?.of?.[~~(Math.random() * item?.of?.length)],
            link: (item) => `<a href="${item.url}"${item.target ? ' target="' + item.target + '"' : ''}>${item.text}</a>`,
            img: (item) => {
                if (item.uri) {
                    return `<a href="${item.uri}"><img src="${item.src}" height=${item.height}px width=${item.width}px></a>`;
                } else {
                    return `<img src="${item.src}" height=${item.height}px width=${item.width}px>`;
                }
            },
            sign: (item) => item.of.map((subitem) => {
                if (subitem.id) {
                    return subitem;
                } else {
                    const str = JSON.stringify(subitem);
                    let hash = 0;
                    for (let i = 0; i < str.length; i++) {
                        const char = str.charCodeAt(i);
                        hash = (hash << 5) - hash + char;
                    }
                    return Object.assign({id: (hash >>> 0).toString(36).padStart(7, '0')}, subitem);
                }
            }),
            'parse query': (item) => item.of.map((subitem) => ({name: subitem.name, query: subitem.query, assembly: this.parse(subitem.query)})),
            results: (item) => this.execute(item.of, this.parse(item.query))
        }
        this.data.annotators = Object.entries(this.annotators).map(([key, code]) => ({id: key, code: code}));
        this.destinations = new Set();
    }
    
    survey() {
        this.destinations = new Set(Object.keys(this.data).concat(this.data?.queries?.map((q) => q.name) || []).concat(Object.keys(this.adapters)));
    }
    
    vals(item) {
        const getname = this.getname;
        const itemvals = [];
        if (Object.keys(item).length > 1) {
            Object.entries(item).filter(([k, v]) => k != 'of').flatMap(([k, v]) => Array.isArray(v) ? v : [v]).map(getname).forEach((v) => itemvals.push(v));
        } else {
            item.of.map(getname).forEach((v) => itemvals.push(v));
        }
        return itemvals;
    }

    numvals(item) {
        return this.vals(item).filter((val) => this.dtype(val, 'number')).map((val) => Number(val));
    }
    
    kkeys(item) {
        return Object.keys(item).filter((k) => k != 'of');
    }

    async querylive(query, inputlist=null) {
        this.recache = new Set();
        const res = await this.query(query, inputlist);
        return res;
    }

    async query(query, inputlist=null, loop=50) {
        this.survey();
        const operations = Array.isArray(query) ? query : this.assemble(this.tokenize(query));
        const result = this.execute(inputlist, operations);
        this.data['current results'] = result;
        const queued = Object.keys(this.adapters).filter((key) => this.adapters[key].queue.length > 0);
        if (loop > 0 && queued.length > 0) {
            await this.adapt();
            if (this.recache) queued.forEach((q) => this.recache.add(q));
            const reres = await this.query(operations, inputlist, loop - 1);
            return reres;
        } else if (loop === 0) {
            this.recache = false;
            Object.keys(this.adapters).forEach((key) => {
                if (!this.adapters[key].annotator) {
                    this.adapters[key].queue.forEach((id) => {
                        (this.index[key] ||= {})[id] = null;
                    })
                }
            })
        }
        this.recache = false;
        return result;
    }
    
    async adapt() {
        for (const key in this.adapters) {
            const adapter = this.adapters[key];
            if (adapter.queue.length > 0) {
                if (adapter.annotator) {
                    while (adapter.queue.length > 0) {
                        this.statusf('annotating ' + key + ' ' + adapter.queue.length);
                        const item_to_annotate = adapter.queue.shift();
                        try {
                            const annotation = await adapter.f(item_to_annotate);
                            (this.index[key] ||= {})[this.getid(item_to_annotate)] = annotation;
                            this.indexlogit(key, 'write');
                        } catch (e) {
                            console.error(e);
                        }                    
                    }
                } else {
                    const newids = adapter.queue.filter((id) => !adapter.pending.has(id));
                    newids.forEach((id) => adapter.pending.add(id));
                    const res = await adapter.f(Array.from(new Set(newids)));
                    newids.forEach((id) => adapter.pending.delete(id));
                    for (const item of res) {
                        const id = this.getid(item);
                        (this.index[key] ||= {})[id] = item;
                        this.indexlogit(key, 'write');
                        if (key in this.data) {
                            this.data[key].push(item);
                        }
                    }
                    this.adapters[key].queue = [];
                }
            }
        }
        this.adaptive = false;
    }
    
    async rehope(only=null) {
        var reset = 0;
        Object.keys(this.index).forEach((k) => {
            if (!only || only == k) {
                const todelete = Object.keys(this.index[k]).filter((dk) => this.index[k][dk] == null || this.index[k][dk] == undefined);
                todelete.forEach((dk) => delete(this.index[k][dk]));
                reset += todelete.length;
            }
        });
        return reset;
    }

    load(something, named, append=false) {
        if (!named) return
        if (!append || !(named in this.data)) this.data[named] = [];
        if (Array.isArray(something)) {
            something.forEach((somethingx) => this.data[named].push(somethingx));
        } else if (typeof something == 'object') {
            const firstkey = Object.keys(something)[0];
            const firstobj = something[firstkey];
            if (typeof firstobj === 'object' && !Array.isArray(firstobj) && (firstkey in Object.values(firstobj) || 'id' in firstobj)) {
                this.data[named].push(...Object.values(something));
            } else if (typeof firstobj === 'object' && !Array.isArray(firstobj)) {
                this.data[named].push(...Object.entries(firstobj).map(([key, val]) => ({id: key, ...val})));
            } else if (typeof firstobj === 'string') {
                this.data[named].push(...Object.entries(something).map(([key, val]) => ({id: key, name: val})));
            } else {
                this.data[named].push(...Object.entries(something).map(([key, val]) => ({id: key, value: val})));
            }
        } else {
            this.data[named].push(something);
        }
        return this.data[named];
    }
    
    async loadjsonl(something, named, append=false) {
        if (!named) return
        if (!append || !(named in this.data)) this.data[named] = [];
        if (typeof something == 'string' && (something.startsWith('http') || something.startsWith('file://'))) {
            const fetchres = await fetch(something);
            something = await fetchres.text();
        }
        var rows=something.trim().split(/[\n\r]+/);
        for (const row of rows) {
            const rowdata = JSON.parse(row);
            if (rowdata) this.data[named].push(rowdata);
        }
    }

    async loadcsv(something, named, quoteChar = '"', delimiter = ',', headerrows=1) {
        if (typeof something == 'string' && (something.startsWith('http') || something.startsWith('file://'))) {
            const fetchres = await fetch(something);
            something = await fetchres.text();
        }
        var rows=something.split(/[\n\r]+/);
    
        const regex = new RegExp(`\\s*(${quoteChar})?(.*?)\\1\\s*(?:${delimiter}|$)`, 'gs');
      
        const match = (line) => Array.from(line.matchAll(regex), (m) => m[2]);

        const headers = [];
        for (let hrowx=0; hrowx<headerrows; hrowx++) {
            const hrow = rows.shift();
            match(hrow).forEach((h, hx) => {
                if (hrowx === 0) {
                    headers.push(h);
                } else {
                    headers[hx] = headers[hx] + ' ' + h;
                }
            });
        }
        const heads = headers.length > 0 ? headers : match(rows.shift());
        var lines = rows.slice(0).filter((line) => line);
        const parsed = lines.map((line) => {
          return match(line).reduce((acc, cur, i) => {
            // replace blank matches with `null`
            const val = cur.length <= 0 ? null : (!isNaN(cur) ? Number(cur) : cur);
            const key = heads[i] ?? `{i}`;
            if (key == '') {
                return { ...acc};
            } else {
                return { ...acc, [key]: val };
            }
          }, {});
        });
        this.load(parsed, named);
    }
    
    apacheLogToDate(apacheTimestamp) {
        // Apache log format: [10/Oct/2000:13:55:36 -0700]
        // Remove brackets if present
        const cleanTimestamp = apacheTimestamp.replace(/^\[|\]$/g, '');
        
        // Split into date/time and timezone parts
        const [dateTimePart, timezone] = cleanTimestamp.split(' ');
        
        // Parse the date/time part: dd/MMM/yyyy:HH:mm:ss
        const [datePart, hour, minute, second] = dateTimePart.split(':');
        const [day, month, year] = datePart.split('/');
        
        // Month mapping
        const months = {
            'Jan': 0, 'Feb': 1, 'Mar': 2, 'Apr': 3, 'May': 4, 'Jun': 5,
            'Jul': 6, 'Aug': 7, 'Sep': 8, 'Oct': 9, 'Nov': 10, 'Dec': 11
        };
        
        // Create Date object (months are 0-indexed in JS)
        const date = new Date(
            parseInt(year),
            months[month],
            parseInt(day),
            parseInt(hour),
            parseInt(minute),
            parseInt(second)
        );
        
        // Handle timezone offset if present
        if (timezone) {
            const sign = timezone[0] === '+' ? 1 : -1;
            const tzHours = parseInt(timezone.slice(1, 3));
            const tzMinutes = parseInt(timezone.slice(3, 5));
            const offsetMs = sign * (tzHours * 60 + tzMinutes) * 60 * 1000;
            
            // Adjust for timezone (Apache logs are in local time, JS Date assumes UTC)
            date.setTime(date.getTime() - offsetMs);
        }
        
        return date;
    }
    
    async loadclf(loglines, named, apiroutes=[]) {
        const lines = loglines.trim().split('\n');
        const cols9 = ['ip', 'name', 'username', 'timestamp', 'requestraw', 'status', 'bytes', 'referrer', 'useragent'];
        const cols10 = ['host'].concat(cols9);
        const parsed = lines.filter((line) => line?.length > 0).map((line) => {
            const vals = Array.from(line.matchAll(/\"(?:\\"|.)*?\"|\[.*?\]|\S+/g)).map((m) => m[0]);
            const cols = vals.length == 10 ? cols10 : cols9;
            const obj = Object.fromEntries(vals.map((v, vx) => ([cols[vx], v])));
            const date = this.apacheLogToDate(obj.timestamp);
            obj.timestamp = date.toISOString();
            [obj.date, obj.time] = obj.timestamp.slice(0, -1).split('T');
            if (obj.requestraw.match(/ [^ ]+ /) && !obj.requestraw.startsWith('"{')) {
                [obj.method, obj.request, obj.protocol] = obj.requestraw.slice(1, -1).split(' ');
                if (this.data['.clf API routes']) {
                    for (const apiroute of this.data['.clf API routes'].sort((a, b) => b.length - a.length)) {
                        if (obj.request.startsWith(apiroute)) {
                            obj.page = apiroute;
                            break;
                        }
                    }
                }
                obj.page ??= obj.request.split('?')[0];
            }
            obj.referrer = obj.referrer.slice(1, -1);
            obj.useragent = obj.useragent.slice(1, -1);
            obj.logline = line;
            return obj;
        });
        this.load(parsed, named);
    }
    
    async loadrss(rsstext, named) {
        const rssval = (k, rawval) => {
            if (k.match(/date/i)) {
                return new Date(rawval).toISOString();
            } else if (!isNaN(rawval)) {
                return Number(rawval);
            } else {
                return rawval;
            }
        }
        const rssdom = new window.DOMParser().parseFromString(rsstext, "text/xml");
        const items = Array.from(rssdom.querySelectorAll('item')).map((i) => {
            const obj = {};
            Array.from(i.children).forEach((c) => {
                const k = c.tagName;
                const rawval = c.textContent.trim();
                obj[k] = rssval(k, rawval);
                if (k.match(/date/i) && obj[k].length > 10 && !('date' in obj)) obj.date = obj[k].slice(0, 10)
            });
            return obj;
        });
        if (named) {
            this.load(items, named);
        } else {
            return items;
        }
    }
    
    async loadopml(opmltext, named) {
        const opmldom = new window.DOMParser().parseFromString(opmltext, "text/xml");
        const feeds = Array.from(opmldom.querySelectorAll('outline[type="rss"]')).map((i) => ({title: i.getAttribute('title'), feed: i.getAttribute('xmlUrl'), site: i.getAttribute('htmlUrl')}));
        if (named) {
            this.load(feeds, named);
        } else {
            return feeds;
        }
    }
    
    connect(key, adapter, doc={}, annotator=null) {
        this.adapters[key] = {queue: [], pending: new Set(), f: adapter, annotator: annotator, doc: doc};
        this.data.adapters ??= [];
        if (!this.data.adapters.find((a) => a.id == key)) this.data.adapters.push({
            id: key,
            requires: doc.requires,
            produces: doc.produces,
            code: this.adapters[key].f
        });
        if (annotator) this.register(key, (item) => this.resolve(key, item), adapter);
    }

    connect_annotator(key, adapter, required=[], doc={}) {
        this.connect(key, adapter, doc, required);
    }
    
    register(key, annotator, displayf=null) {
        this.annotators[key] = annotator;
        this.data.annotators ??= [];
        if (!this.data.annotators.find((a) => a.id == key)) this.data.annotators.push({id: key, code: displayf || this.annotators[key]});
    }

    unbracket(token) {
        if (!(typeof token == 'string')) token = token.toString();
        if (token.startsWith('[') && token.endsWith(']')) {
            return token.slice(1, -1).replaceAll(']]', ']');
        }
        return token;
    }

    bracket(token) {
        if (!(typeof token == 'string')) token = token?.toString() ?? '';
        if (token.match(/[?.:#\/|!<>=~@\[\]\(\),;\+-]/) || token.startsWith(' ') || token.endsWith(' ')) {
            return '[' + token.replaceAll(']', ']]') + ']';
        }
        return token;
    }

    escapeRegExp(string) {
        return string.replace(/[.*+?${}()|[\]\\]/g, "\\$&");
    }

    tokenize(text) {
        if (typeof text != 'string') text = String(text);
        const matches = text.matchAll(/(\[(?:\]\]|[^\]])*\])|(\?{1,3})|(\.{1,4})|(\/{1,2})|([\:\#\|\!])|(\()|(\))|([@~=<>\+-]+)|(\,)|(\;)|([^\[\]\(\)\.\?\:\/\#\|\!~=<>@,;\+-]+)|([\[\]])/gms);
        const tokenlist = Array.from(matches, m => m[0].trim()).filter((token) => token.length > 0);
        return tokenlist;
    }

    assemble(tokenized) {
        const isOperator = (token) => '???....://#|!'.includes(token);
        const tokenlist = '= ~ =~ ~< ~> > < <> >< >= <= - =- ~- + =+ @ @@ @- @= @@= =@ =@@ @< @@< @<= @@<= @> @@> @>= @@>= =>'.split(' ');
        const isSubop = (token) => tokenlist.includes(token) || (token?.startsWith('-') && (token.length == 1 || tokenlist.includes(token.slice(1))));
        const isSeparator = (token) => ',;'.includes(token);
        const isValue = (token) => !isOperator(token) && !isSubop(token) && !isSeparator(token);
                
        let tokens = tokenized.slice();
        let level = 0;
        const operations = [];
        while (tokens.length > 0) {
            const op = {operator: null, args: []};
            let token = tokens.shift();
            if (isOperator(token) || ((isValue(token) || isSubop(token)) && operations.length === 0)) {
                if ((isValue(token) || isSubop(token)) && operations.length === 0) {
                    op.operator = '?';
                    tokens.unshift(token);
                } else {
                    op.operator = token;
                }
                while (tokens.length > 0 && !isOperator(tokens[0])) {
                    const arg = {separator: null, label: null, subop: null, value: null};
                    if (isSeparator(tokens[0])) {
                        arg.separator = tokens.shift();
                    }
                    while (tokens.length > 0 && !isOperator(tokens[0]) && !isSeparator(tokens[0])) {
                        const frag = tokens.shift();
                        if (frag != '(' && isValue(frag) && isSubop(tokens[0])) {
                            arg.label = this.unbracket(frag);
                            arg.subop = tokens.shift();
                        } else if (isSubop(frag)) {
                            arg.subop = frag;
                        } else if (frag === '(') {
                            const subquery = [];
                            level++;
                            while (tokens.length > 0 && level > 0) {
                                const sub = tokens.shift();
                                if (sub === '(') {
                                    level++;
                                    if (level > 0) {
                                        subquery.push(sub);
                                    }
                                } else if (sub === ')') {
                                    level--;
                                    if (level > 0) {
                                        subquery.push(sub);
                                    }
                                } else if (level > 0) {
                                    subquery.push(sub);
                                }
                            }
                            const subexpr = this.assemble(subquery);
                            arg.value = subexpr;
                        } else if (frag.match(/^[=<>@~-]+$/)) {
                            const failure = {tokens: tokenized, assembled: operations.slice(0), assembling: {op: op, arg: arg, unexpected: frag}, unassembled: tokens.slice(0)}
                        throw new Error("Invalid subop", {cause: failure});
                        } else if (!arg.value) {
                            arg.value = this.unbracket(frag);
                        } else {
                            const failure = {tokens: tokenized, assembled: operations.slice(0), assembling: {op: op, arg: arg, unexpected: frag}, unassembled: tokens.slice(0)}
                            throw new Error("Unexpected token", {cause: failure});
                        }
                    }
                    op.args.push(arg);
                }
            }
            operations.push(op);
            
        }
        if (level > 0) console.warn({parentropy: level, operations: operations});
        return operations;
    }
    
    parse(querystr) {
        return this.assemble(this.tokenize(querystr));
    }
    
    disassemble(assembly) {
        return assembly.map((operation, ox) => operation.operator + ((operation.args.length == 0 && ox < assembly.length - 1 && operation.operator[0] == assembly[ox + 1].operator[0]) ? ' ' : operation.args.map((arg) => (arg.separator ?? '') + (arg.label ? this.bracket(arg.label) : '') + (arg.subop ?? '') + (Array.isArray(arg.value) ? ('(' + this.disassemble(arg.value) + ')') : (arg.value ? this.bracket(arg.value) : ''))).join(''))).join('').replace(/^\?(?=[a-z])/, '');
    }
    
    compact(querystr) {
        return this.disassemble(this.parse(querystr));
    }
    
    executeq(querystr) {
        return this.execute([], this.assemble(this.tokenize(querystr)));
    }
    
    timecheck(timer, i, count, op) {
        if (i == 1) timer.loopstart = new Date();
        if (i >= 10 && i >= count / 100) {
            const taken = new Date() - timer.loopstart;
            const projected = count * taken / i;
            if (this.timelimit && projected > this.timelimit) throw new Error('Query overrun.', {cause: {operation: this.disassemble([op]), items: count, done: i, elapsed: taken + 'ms', projected: Math.round(projected / 60000) + ' minutes', timelimit: Math.round(this.timelimit / 60000) + ' minutes'}});
        }
    }
    
    unmmss = (mmssstr) => {
        const parts = mmssstr.split(':');
        let s = 0;
        const seconds = parts.pop();
        if (seconds) s += Number(seconds);
        const minutes = parts.pop();
        if (minutes) s += 60 * Number(minutes);
        const hours = parts.pop();
        if (hours) s += 60 * 60 * Number(hours);
        return s;
    }
    
    dethe = (value) => {return value.toLowerCase().replace(/^the /, '')};

    compvals = (araw, braw) => {  
        const a = araw.toString().trim();
        const b = braw.toString().trim();  
        const atime = a.match(/^(?:\d+)(?:\:\d{2,})+$/);
        const btime = b.match(/^(?:\d+)(?:\:\d{2,})+$/);
        if (atime && btime) {
            return this.unmmss(btime[0]) - this.unmmss(atime[0]);
        }
        return this.dethe(a).localeCompare(this.dethe(b));
    }
    
    execute(inputlistraw, operations, labeled=null, level=null) {
        let inputlist = Array.isArray(inputlistraw) ? inputlistraw : inputlistraw ? [inputlistraw] : [];
        let currentlist = inputlist?.slice(0) || [];
        const getname = this.getname;
        const getid = this.getid;
        const dtype = this.dtype;
        const nullish = this.nullish;
        const dcopy = this.dcopy;
        const step = this.step;
        const dethe = this.dethe;
        const compvals = this.compvals;
            
        labeled ||= {};
        let outputlist = [];
        const toplevel = !inputlistraw && !level;
        if (toplevel && operations[0]?.progress) {
            currentlist = operations[0].progress;
            labeled = operations[0].labeled;
            outputlist = currentlist;
        }
        for (let opx=0; opx<operations.length; opx++) {
            const op = operations[opx];
            if (op.completed) continue;
            const opstart = performance.now();
            outputlist = [];
            switch (op.operator) {
                case '?': // start
                    if (op.args.length == 0) {
                        outputlist = Object.keys(this.data).filter((x) => !this.internal_datasets.includes(x)).sort((a, b) => dethe(a).localeCompare(dethe(b)));
                        break;
                    }
                    outputlist = [];
                    op.args.forEach((arg) => {
                        let startitems = [];
                        if (arg.subop?.includes('+')) {
                            if (arg.label) {
                                labeled[arg.label].forEach((i) => outputlist.push(i));
                            } else {
                                currentlist.forEach((i) => outputlist.push(i));
                            }
                        }
                        let typeitems;
                        if (arg.separator != ';' || outputlist.length == 0) {
                            if (Array.isArray(arg.value)) {
                                startitems = this.execute(arg.subop?.includes('+') ? outputlist : [], arg.value, labeled);
                            } else if (arg.subop == '~') {
                                startitems = [arg.value];
                            } else if (arg.value in labeled) {
                                startitems = labeled[arg.value];
                            } else if (typeitems = this.gettype(arg.value)) {
                                startitems = typeitems;
                            } else if (arg.subop != '=') {
                                if (!isNaN(arg.value)) {
                                    startitems = [Number(arg.value)];
                                } else {
                                    startitems = [arg.value];
                                }
                            }
                            if (arg.label) this.index[arg.label] = {};
                            if (arg.subop?.includes('+')) {
                                if (arg.label) {
                                    labeled[arg.label] ||= [];
                                    startitems.forEach((i) => {
                                        labeled[arg.label].push(i);
                                    })
                                    outputlist = labeled[arg.label];
                                } else {
                                    startitems.forEach((i) => outputlist.push(i));
                                }
                            } else if (arg.subop?.includes('-')) {
                                const startids = new Set(startitems.map((si) => getid(si)));
                                if (arg.label && labeled[arg.label]) {
                                    labeled[arg.label] = labeled[arg.label].filter((li) => !startids.has(getid(li)));
                                    outputlist = labeled[arg.label];
                                } else {
                                    outputlist = outputlist.filter((li) => !startids.has(getid(li)));
                                }
                            } else {
                                if (arg.label) labeled[arg.label] = startitems;
                                startitems.forEach((si) => outputlist.push(si));
                            }
                        }
                    });
                    break;
                case '??': // label
                    outputlist = currentlist;
                    let ended = false;
                    op.args.forEach((arg) => {
                        if (!ended) {
                            if (arg.label == '_timelimit' && !isNaN(arg.value)) {
                                this.timelimit = Number(arg.value) * 60000;
                            } else if (arg.label == 'status') {
                                if (arg.value) {
                                    this.statusf(arg.value);
                                } else {
                                    this.statusf(currentlist[0]);
                                }
                            } else if (arg.label) {
                                if (arg.subop.includes('~') && dtype(arg.value, 'string')) {
                                    labeled[arg.label] = [arg.value];
                                // } else if (arg.labeled) {
                                //     labeled[arg.label] = arg.labeled;
                                } else {
                                    if (arg.subop?.match(/\+/)) labeled[arg.label] ||= [];
                                    const newvals = this.execute(currentlist, Array.isArray(arg.value) ? arg.value : '.' + arg.value, labeled, level);
                                    if (arg.subop?.match(/\+/)) {
                                        newvals.forEach((nv) => labeled[arg.label].push(nv));
                                    } else if (arg.subop?.includes('-') && labeled[arg.label]) {
                                        const newids = new Set(newvals.map((ni) => getid(ni)));
                                        labeled[arg.label] = labeled[arg.label].filter((li) => !newids.has(getid(li)));
                                    } else {
                                        labeled[arg.label] = newvals;
                                    }
                                    // if (!level && opx == 0) arg.labeled = newvals;
                                }
                                this.index[arg.label] = {};
                            } else if (dtype(arg.value, 'string')) {
                                if (arg.subop?.match(/\+/)) {
                                    labeled[arg.value] ??= [];
                                    const currentids = new Set(currentlist.map((ci) => getid(ci)));
                                    currentlist.forEach((i) => labeled[arg.value].push(i));
                                } else if (arg.subop?.includes('-') && labeled[arg.value]) {
                                    const currentids = new Set(currentlist.map((ci) => getid(ci)));
                                    labeled[arg.value] = labeled[arg.value].filter((li) => !currentids.has(getid(li)));
                                } else {
                                    labeled[arg.value] = currentlist.slice(0);
                                }
                                this.index[arg.value] = {};
                                if (arg.value == 'end') {
                                    ended = true;
                                }
                            }
                        }
                    });
                    if (ended) return (this.debug && !inputlist) ? operations : outputlist;
                    break;
                case '!': // repeat
                    if (opx > 0) {
                        const repeat_ops = operations.slice(opx - 1, opx + 1);
                        repeat_ops.forEach((rop) => {
                            delete(rop.progress);
                            delete(rop.labeled);
                            delete(rop.completed);
                        });
                        outputlist = currentlist.slice(0);
                        level ??= 0;
                        level += 1;
                        const maxrecursion = ((op.args.length > 0 && op.args[0].value) || 1000);
                        if (outputlist.length > 0 && level < maxrecursion && (!this.samearray(inputlist, outputlist) || level == 1)) {
                            const recursed = this.execute(outputlist, repeat_ops, labeled, level);
                            if (recursed.length > 0 && !this.samearray(recursed, outputlist)) outputlist = recursed.slice(0);
                        }
                    }
                    break;
                case '.': // traverse
                case '..': // traverse with duplicates
                    const seen = new Set();
                    const sofar =  Array.isArray(op.args?.[0]?.value) && op.args?.[0]?.label;
                    let transq = null;
                    if (sofar) labeled[sofar] = [];
                    if (op.operator == '.' && op.args?.length == 1) {
                        const firstarg = op.args[0];
                        const firstval = firstarg?.value;
                        if (dtype(firstval, 'string')) {
                            transq = this.data.queries?.find((q) => q.relative == true && q.name == firstval);
                        }
                    }
                    if (transq) {
                        outputlist = this.execute(currentlist, this.parse(transq.query), labeled, level);
                    } else {
                        const traversetimer = {};
                        outputlist = currentlist.reduce((acc, item, i) => {
                            this.timecheck(traversetimer, i, currentlist.length, op);
                            if (op.args.length == 0) {
                                const itemkey = getid(item);
                                if (op.operator == '..' || !seen.has(itemkey)) {
                                    acc.push(item);
                                    seen.add(itemkey);
                                }
                                return acc;
                            }
                            if (op.args[0].subop?.includes('<') && acc.length > 0) return acc;
                            let itemvals = [];
                            let toremoveids = {};
                            for (const arg of op.args.filter((arg) => arg.value != null && arg.value != undefined)) {
                                var passdown = null;
                                if (arg.subop?.includes('>')) {
                                    if (arg?.label in item) {
                                        passdown = item[arg.label];
                                    } else {
                                        passdown = JSON.parse(JSON.stringify(item, Object.keys(item).filter((k) => k != arg.value)));
                                    }
                                }
                                if (arg.separator != ';' || itemvals.length == 0) {
                                    let itemval;
                                    if (arg.subop?.includes('@') && arg.label != null && !isNaN(arg.value)) {
                                        const itemvalraw = step(item, arg.label, null, labeled);
                                        if (arg.subop.includes('@@')) {
                                            itemval = itemvalraw.slice(itemvalraw.length - Number(arg.value));
                                        } else {
                                            itemval = itemvalraw.slice(0, Number(arg.value));
                                        }
                                    } else {
                                        itemval = step(item, arg.value, arg.subop, labeled);
                                    }
                                    if (arg.subop?.includes('-') && isNaN(arg.value)) {
                                        itemval.forEach((subitem) => {
                                            const subid = getid(subitem);
                                            toremoveids[subid] ||= 0;
                                            toremoveids[subid] += 1;
                                        });
                                    } else {
                                        itemval.forEach((subitem) => {
                                            if (passdown) {
                                                if (!dtype(subitem, 'object')) subitem = {value: subitem};
                                                if (arg.label) {
                                                    subitem[arg.label] = Array.isArray(passdown) ? passdown : [passdown];
                                                } else {
                                                    Object.assign(subitem, passdown);
                                                }
                                            }
                                            const subitemkey = getid(subitem);
                                            if (op.operator == '..' || !seen.has(subitemkey)) {
                                                seen.add(subitemkey);
                                                itemvals.push(subitem);
                                                if (sofar) labeled[sofar].push(subitem);
                                            }
                                        })
                                    }
                                }
                                if (Object.keys(toremoveids).length > 0) {
                                    if (op.operator == '.') {
                                        itemvals = itemvals.filter((subitem) => !(getid(subitem) in toremoveids));
                                    } else {
                                        itemvals = itemvals.filter((subitem) => {
                                            const subid = getid(subitem);
                                            if (toremoveids[subid] > 0) {
                                                toremoveids[subid] -= 1;
                                                return false;
                                            } else {
                                                return true;
                                            }
                                        })
                                    }
                                }
                            }
                            itemvals.forEach((x) => acc.push(x));
                            return acc;
                        }, []);
                    }
                    outputlist = outputlist.filter((item) => item != null);
                    break;
                case ':': // filter
                    if (!(op?.args?.length > 0)) {
                        outputlist = currentlist;
                        break;
                    }
                    
                    if (op?.args?.length == 1 && op.args[0].subop && op.args[0].label == null && op.args[0].value == null) {
                        let checklist;
                        if (currentlist.length == 0) {
                            outputlist = [];
                            break;
                        } else if (currentlist.length == 1) {
                            checklist = [currentlist[0], currentlist[0]];
                        } else {
                            checklist = currentlist;
                        }
                        let comparator = op.args[0].subop;
                        let polarize = (x) => x;
                        let check;
                        if (comparator?.startsWith('-') || comparator?.endsWith('-')) {
                            polarize = (x) => !x;
                            comparator = comparator.replace(/-|-$/, '');
                        }
                        for(let pi=0; pi<checklist.length-1; pi++) {
                            let a = getname(checklist[pi]);
                            let b = getname(checklist[pi+1]);
                            switch (comparator) {
                                case '': check = polarize(a == b); break;
                                case '=': check = polarize(a == b); break;
                                case '>=': check = polarize(a >= b); break;
                                case '<=': check = polarize(a <= b); break;
                                case '>': check = polarize(a > b); break;
                                case '<': check = polarize(a < b); break;
                                case '~': check = polarize(a?.toString().toLowerCase().includes(b?.toString().toLowerCase())); break;
                                case '~<': check = polarize(a?.toString().toLowerCase().startsWith(b?.toString().toLowerCase())); break;
                                case '~>': check = polarize(a?.toString().toLowerCase().endsWith(b?.toString().toLowerCase())); break;
                            }
                            if (!check) {
                                outputlist = [];
                                break;
                            }
                        }
                        if (check) outputlist = currentlist;
                        break;
                    }
                    
                    for(const arg of op.args) {
                        if ([null, '', '=', '~'].includes((arg.subop || '').replace(/-/, '')) && dtype(arg.value, 'string') && arg.value.startsWith('~')) {
                            const flags = arg.value.startsWith('~~') ? 'i' : '';
                            const pattern = arg.value.replace(/^~*/, '');
                            const fullpattern = (arg.subop || '').replace(/-/, '') == '=' ? ((pattern.startsWith('^') ? '' : '^') + pattern + (pattern.endsWith('$') ? '' : '$')) : pattern;
                            arg.re = new RegExp(fullpattern, flags);
                        }
                    }

                    const ands = [[]];
                    for (const arg of op.args) {
                        if (arg.separator == ';') {
                            ands.push([arg]);
                        } else {
                            ands[ands.length - 1].push(arg)
                        }
                    }
                    const filtertimer = {};
                    outputlist = currentlist.filter((item, i) => {
                        this.timecheck(filtertimer, i, currentlist.length, op);
                        return ands.filter((and) => {
                            return and.filter((arg) => {
                                let comparator = arg.subop;
                                let polarize = (x) => x;
                                if (comparator?.startsWith('-') || comparator?.endsWith('-')) {
                                    polarize = (x) => !x;
                                    comparator = comparator.replace(/-|-$/, '');
                                }
                                if (!comparator && arg.label == null && Array.isArray(arg.value)) {
                                    return polarize(this.execute([item], arg.value, labeled).length > 0);
                                } else if (['+', ''].includes(comparator) && arg.label && !arg.value && dtype(arg.label, 'string') && dtype(item, 'object')) {
                                    const propval = item[arg.label];
                                    // console.log({plusminus: comparator, label: arg.label, propval: propval, judgment: polarize(Array.isArray(propval) ? propval.length > 0 : propval)})
                                    return polarize(Array.isArray(propval) ? propval.length > 0 : propval);
                                }

                                let testitems = [item];
                                if (arg.label && dtype(item, 'object')) {
                                    testitems = step(item, arg.label, null, labeled);
                                }

                                const testvals = testitems.map((testitem) => {
                                    if (comparator || arg.re) {
                                        return getname(testitem);
                                    } else if (this.dtype(testitem, 'literal')) {
                                        return testitem;
                                    } else if ('id' in testitem) {
                                        return testitem.id;
                                    } else {
                                        return getname(testitem)
                                    }
                                });

                                let argvals;
                                if (Array.isArray(arg.value) || arg.value?.startsWith('=')) {
                                    const argvalitems = this.step(item, arg.value, null, labeled);
                                    argvals = argvalitems.map((argvalitem) => {
                                        let argval;
                                        if (dtype(argvalitem, 'literal')) {
                                            argval = argvalitem;
                                        } else {
                                            argval = getname(argvalitem);
                                        }
                                        if (argval == null) {
                                            const argvalitemkeys = Object.keys(argvalitem).filter((key) => key != 'id');
                                            if (argvalitemkeys.length == 1) {
                                                argval = argvalitem[argvalitemkeys[0]];
                                            }
                                        }
                                        return argval;
                                    });
                                }

                                return testvals.find((testval) => {
                                    if (!argvals) {
                                        if (dtype(testval, 'number') && dtype(arg.value, 'number')) {
                                            testval = Number(testval);
                                            argvals = [Number(arg.value)];
                                        } else {
                                            argvals = [arg.value];
                                        }
                                    }
                                    if (comparator?.startsWith('@')) {
                                        if (dtype(arg.value, 'number')) {
                                            argvals = [Number(arg.value)];
                                        } else if (arg.value in labeled) {
                                            argvals = Number(labeled[arg.value]);
                                            if (!Array.isArray(argvals)) argvals = [argvals];
                                        }
                                        if (comparator.startsWith('@@')) {
                                            testval = currentlist.length - i;
                                            comparator = comparator.slice(2);
                                        } else {
                                            testval = i + 1;
                                            comparator = comparator.slice(1);
                                        }
                                    }
                                    if (testval == null || argvals == null || argvals.length == 0) return polarize(false);
                                    return argvals.find((argval) => {
                                        if (arg.re) {
                                            return polarize(testval.match(arg.re));
                                        } else {
                                            switch (comparator) {
                                                case null: return polarize(testval == argval);
                                                case '': return polarize(testval == argval);
                                                case '=': return polarize(testval == argval);
                                                case '>=': return polarize(testval >= argval);    
                                                case '<=': return polarize(testval <= argval);
                                                case '>': return polarize(testval > argval);
                                                case '<': return polarize(testval < argval);
                                                case '~': return polarize(testval?.toString().toLowerCase().includes(argval?.toString().toLowerCase()));
                                                case '~<': return polarize(testval?.toString().toLowerCase().startsWith(argval?.toString().toLowerCase()));
                                                case '~>': return polarize(testval?.toString().toLowerCase().endsWith(argval?.toString().toLowerCase()));
                                            }
                                        }
                                    }) != null;
                                }) != null;
                            }).length > 0;
                        }).length == ands.length;
                    });
                    break;
                case '#': // sort
                    const sortargs = op.args.slice(0);
                    const lastarg = sortargs[sortargs.length - 1];
                    let temped = false;
                    if (!lastarg || ![';', '=;'].includes(Object.values(lastarg).join(''))) sortargs.push(...[{subop: null, value: 'name'}, {subop: null, value: 'id'}]);
                    if (!dtype(currentlist[0], 'object') && lastarg && lastarg.separator == ';') {
                        currentlist = currentlist.map((v) => ({_value: v}));
                        temped = true;
                    } else if (sortargs[0]?.label) {
                        currentlist = currentlist.map((i) => dcopy(i));
                    }
                    sortargs.forEach((arg) => {
                        arg.extraindex = {};
                        const stablesort = (arg.label == null && arg.value == null && arg.separator == ';' ) ? (arg.subop == '-' ? -1 : 1) : null;
                        if (arg.subop?.includes('~')) {
                            arg.sortmode = 'literal';
                        } else if (arg.subop?.endsWith('-') || arg.subop?.endsWith('>')) {
                            arg.sortmode = 'numeric';
                        } else if (arg.subop?.endsWith('+') || arg.subop?.endsWith('<')) {
                            arg.sortmode = 'rank';
                        } else {
                            arg.sortmode = stablesort || ['rank', 'index', 'number', 'id'].includes(arg.value) ? 'rank' : 'numeric';
                        }
                        const vals = new Set();
                        const extradone = new Set();
                        for (let ix=0; ix<currentlist.length; ix++) {
                            const item = currentlist[ix];
                            const vallist = stablesort ? [ix] : (arg.value ? step(item, arg.value, null, labeled) : (Array.isArray(item) ? item : [item]));
                            if (vallist && !stablesort) vallist.forEach((val) => vals.add(val));
                            if (typeof item == 'object') {
                                (item._sortindex ||= []).push(vallist);
                            } else if (!dtype(item, 'number') && !extradone.has(item)) {
                                arg.extraindex[item] = vallist;
                                extradone.add(item);
                            }
                        };
                        if (arg.sortmode != 'literal') {
                            for (const val of vals) {
                                if (val != null && !dtype(val, 'number') && !(dtype(val, 'object') && dtype(getname(val), 'number'))) {
                                    arg.sortmode = null;
                                    break;
                                }
                            };
                        }
                    })
                    
                    outputlist = currentlist.sort((a, b) => {
                        let comp = 0;
                        let ii = 0;
                        for (const arg of sortargs) {
                            const alist = typeof a == 'object' ? a._sortindex[ii] : dtype(a, 'number') ? [Number(a)] : arg.extraindex[a];
                            const blist = typeof b == 'object' ? b._sortindex[ii] : dtype(b, 'number') ? [Number(b)] : arg.extraindex[b];
                            ii++;
                            if (alist && blist) {
                                if (arg?.subop?.includes('@')) {
                                    const ifunc = arg.subop == '@=' ? getname : getid;
                                    const alookup = alist.indexOf(ifunc(a));
                                    const blookup = blist.indexOf(ifunc(b));
                                    if (alookup >-1 && blookup == -1) {
                                        comp = -1;
                                    } else if (alookup == -1 && blookup > -1) {
                                        comp = 1;
                                    } else {
                                        const aliststrs = alist.map((ax) => ax.toString());
                                        comp = aliststrs.indexOf(ifunc(a).toString()) - aliststrs.indexOf(ifunc(b).toString());
                                    }
                                } else {
                                    for (let i=0; i < Math.min(alist.length, blist.length); i++) {
                                        let aitem = alist[i];
                                        let bitem = blist[i];
                                        const anull = nullish(aitem);
                                        const bnull = nullish(bitem);
                                        if (anull && bnull) {
                                            comp = 0;
                                        } else if (bnull) {
                                            comp = -1;
                                        } else if (anull && !bnull) {
                                            comp = 1;
                                        } else {
                                            switch (arg.sortmode) {
                                                case 'literal':
                                                    const atrim = typeof aitem == 'string' ? aitem.trim() : aitem;
                                                    const btrim = typeof bitem == 'string' ? bitem.trim() : bitem;
                                                    comp = atrim < btrim ? -1 : (btrim < atrim ? 1 : 0);
                                                    break;
                                                case 'numeric':
                                                    let bnum = Number(bitem);
                                                    let anum = Number(aitem);
                                                    if (isNaN(bnum) || isNaN(anum)) {
                                                        bnum = Number(getname(bitem));
                                                        anum = Number(getname(aitem));
                                                    }
                                                    if (isNaN(bnum) || isNaN(anum)) {
                                                        comp = compvals(aitem, bitem)
                                                    } else {
                                                        comp = bnum - anum;
                                                    }
                                                    break;
                                                case 'rank':
                                                    comp = Number(aitem) - Number(bitem);
                                                    break;
                                                default:
                                                    if (dtype(aitem, 'literal') && dtype(bitem, 'literal')) {
                                                        comp = compvals(aitem, bitem);
                                                    } else if (typeof aitem == 'object' && typeof bitem == 'object') {
                                                        let aval = getname(aitem);
                                                        let bval = getname(bitem);
                                                        if (!nullish(aval) && !nullish(bval)) {
                                                            comp = compvals(aval, bval);
                                                        } else {
                                                            aval = getid(aitem);
                                                            bval = getid(bitem);
                                                            if (!nullish(aval) && !nullish(bval)) {
                                                                comp = compvals(aval, bval);
                                                            } else {
                                                                comp = 0;
                                                            }
                                                        }
                                                    }
                                                    break;
                                            }
                                        }
                                        if (comp != 0) break;
                                    }
                                }
                            }
                            if (comp === 0) {
                                if (alist && blist) {
                                    comp = blist.length - alist.length;
                                } else if (alist) {
                                    comp = -1;
                                } else if (blist) {
                                    comp = 1;
                                }
                            }
                            if (arg?.subop?.includes('-') && (arg.sortmode == 'literal' || !arg.sortmode)) {
                                comp = -comp;
                            }
                            if (comp != 0) break;
                        }
                        return comp;
                    })
                    if (temped) outputlist = outputlist.map((vt) => vt._value);
                    outputlist.forEach((item, i) => {
                        delete item._sortindex;
                        if (op.args.length > 0 && op.args[0].label) item[op.args[0].label] = i + 1;
                    });
                    break;
                case '/': // group
                case '//': // merge
                    const groupindex = new Map();
                    let keylists = {};
                    let ofname = 'of';
                    let countname = 'count';
                    let sortgroups = true;
                    const groupargs = [];
                    const accumulates = {};
                    const discards = new Set();
                    const merge_ands = [];
                    for (const arg of op.args) {
                        if (op.operator == '//' && arg.subop?.includes('+') && typeof arg.value == 'string') {
                            accumulates[arg.value] = arg.label || arg.value;
                        } else if (op.operator == '//' && arg.subop?.includes('-') && typeof arg.value == 'string') {
                            discards.add(arg.value);
                        } else if (arg.separator == ';' && arg.label == null && arg.value == null) {
                            sortgroups = false;
                        } else if (op.operator == '//' && (arg.separator == ';' || merge_ands.length > 0)) {
                            if (arg.separator == ';') {
                                merge_ands.push([arg.value]);
                            } else {
                                merge_ands[merge_ands.length - 1].push(arg.value)
                            }
                        } else {
                            groupargs.push(arg);
                        }
                    }
                    if (groupargs.length == 0) groupargs.push({value: null});
                    
                    groupargs.forEach((arg) => arg.groupcounter = 0);
                    
                    const grouptimer = {};
                    for (let ix=0; ix<currentlist.length; ix++) {
                        this.timecheck(grouptimer, ix, currentlist.length, op);
                        const item = currentlist[ix];
                        let keys = null;
                        let keyi = 0;
                        let itemsleft = currentlist.length - ix;
                        for (const arg of groupargs) {
                            if (arg.label == 'of') {
                                ofname = arg.value;
                                continue;
                            } else if (arg.label == 'count') {
                                countname = arg.value;
                                continue;
                            }
                            keyi++;
                            let groupnumber = null;
                            if (op.operator == '/' && dtype(arg.value, 'number')) {
                                if (arg.subop?.endsWith('@')) {
                                    arg.divisor = Number(arg.value);
                                } else {
                                    arg.divisor = currentlist.length / Number(arg.value);   
                                }
                                groupnumber = Math.floor(ix / arg.divisor) + 1;
                            } else if (arg.value != null && arg.subop?.endsWith('@@')) {
                                const groupval = step(item, arg.value, null, labeled);
                                if (ix == 0 || groupval?.length > 0) arg.groupcounter += 1;
                                groupnumber = arg.groupcounter;
                            }
                            const label = arg.label ?? (typeof arg.value === 'string' ? arg.value : null) ?? keyi;
                            const newkeyitems = groupnumber != null ? [groupnumber] : arg.value ? step(item, arg.value, arg.subop, labeled) : [null];
                            const newkeys = newkeyitems.map((newkey) => ([{arglabel: arg.label, label: label, keyitem: this.resolve(arg.value, newkey, labeled)}]));
                            if (keys) {
                                keys = keys.flatMap((oldkeys) => newkeys.filter((newkey) => arg.separator == ',' || oldkeys.filter((oldkey) => compvals(getname(oldkey.keyitem) || oldkey.keyitem, getname(newkey[0].keyitem) || newkey[0].keyitem) >= 0).length === 0).map((newkey) => oldkeys.concat(newkey)));
                            } else {
                                keys = newkeys;
                            }
                            if (arg.subop?.endsWith('@') || dtype(arg.value, 'number')) {
                                keys.forEach((key) => {
                                    const testkey = JSON.stringify(key.slice(0, -1));
                                    const newkeytest = JSON.stringify(key[key.length - 1]);
                                    if (testkey in keylists) {
                                        const lastkey = keylists[testkey][keylists[testkey].length - 1];
                                        if (newkeytest != lastkey) keylists[testkey].push(newkeytest);
                                    } else {
                                        keylists[testkey] = [newkeytest];
                                    }
                                    key[key.length - 1].keyindex = keylists[testkey].length;
                                });
                            }
                        }
                        if (keys) {
                            for (const key of keys) {
                                const keystr = JSON.stringify(key);
                                if (!groupindex.has(keystr)) groupindex.set(keystr, []);
                                groupindex.get(keystr).push(item);
                            }
                        }
                    }
                    for (const [keystr, items] of groupindex) {
                        if (op.operator == '/') {
                            const newgroup = {};
                            const keydata = JSON.parse(keystr);
                            if (keydata.length == 1 && keydata[0].keyitem != null) {
                                if (keydata[0].keyindex) {
                                    newgroup.keyindex = keydata[0].keyindex;
                                } else {
                                    let keyitemname = getname(keydata[0].keyitem);
                                    if (keyitemname != null) {
                                        newgroup.name = keyitemname;
                                    }
                                }
                            }
                            let skip = false;
                            let keys = [];
                            for (const {arglabel, label, keyitem, keyindex} of keydata) {
                                if (keyitem != null) {
                                    const keyobj = (dtype(keyitem, 'object') || keyindex == null || keyindex == undefined) ? keyitem : (keyindex != null && keyindex != undefined) ? {} : {name: keyitem};
                                    if (keyindex != null) {
                                        keyobj.keyindex = keyindex;
                                    }
                                    keys.push(keyobj)
                                    if (label && dtype(label, 'string') && isNaN(label) && label != '_') {
                                        newgroup[label] = [keyobj];
                                    }
                                }
                            }
                            newgroup[countname] = items.length;
                            if (keys) newgroup.key = keys;
                            newgroup[ofname] = items;
                            outputlist.push(newgroup);
                        } else if (op.operator == '//') {
                            const groupprops = groupargs.map((grouparg) => grouparg.value).filter((gp) => gp);
                            if (merge_ands.length == 0 || !merge_ands.find((mand) => !mand.find((mr) => items.find((item) => mr in item)))) {
                                const newgroup = items.reduce((acc, item) => {
                                    const proporder = groupprops.slice(0);
                                    Object.keys(item).forEach((prop) => {
                                        if (!proporder.includes(prop) && !discards.has(prop)) proporder.push(prop);
                                    });
                                    for(const prop of proporder) {
                                        const writeprop = accumulates[prop] || prop;
                                        if (writeprop in acc) {
                                            if (Array.isArray(acc[writeprop])) {
                                                (Array.isArray(item[prop]) ? item[prop] : [item[prop]]).filter((val) => !acc[writeprop].includes(val)).forEach((val) => acc[writeprop].push(val));
                                            }
                                        } else {
                                            if (prop in accumulates && !Array.isArray(item[prop])) {
                                                acc[writeprop] = [item[prop]];
                                            } else {
                                                acc[writeprop] = item[prop];
                                            }
                                        }
                                    }
                                    return acc;
                                }, {});
                                const keydata = JSON.parse(keystr);
                                for (const {arglabel, label, keyitem, keyindex} of keydata) {
                                    if (arglabel && typeof arglabel === 'string' && arglabel != '_') {
                                        newgroup[arglabel] = [keyitem];
                                    }
                                }
                                outputlist.push(newgroup);
                            }
                        }
                    }
                    if (sortgroups) {
                        const sortquery = '#' + groupargs.map((arg, argx) => (arg.subop?.endsWith('@') || arg.divisor ? '+' : '') + '(..key:@' + (argx + 1) + (arg.subop?.endsWith('@') ? '.keyindex;_' : '') + ')').join(',');
                        outputlist = this.execute(outputlist, this.assemble(this.tokenize(sortquery)), labeled);
                    }
                    break;
                case '...': // synthesize
                case '....': // synthesize and extract
                    if (!(op?.args?.length > 0)) {
                        if (op.operator == '...') {
                            outputlist = [{of: currentlist.map((item) => dcopy(item))}];
                        } else {
                            outputlist = [currentlist.length]
                        }
                        break;
                    }
                    const segments = [[]];
                    op.args.forEach((arg) => {
                        if (arg.separator == ';') segments.push([]);
                        segments[segments.length - 1].push(arg);
                    });
                    outputlist = [];
                    let labels = null;
                    if (
                        segments.length > 1 &&
                        segments[0].filter((s) => s.label && s.subop && s.value == null).length == segments[0].length &&
                        segments.slice(1).filter((s) => s.length == segments[0].length).length == segments.length - 1
                    ) {
                        labels = segments.shift();
                    }
                    for (const segment of segments) {
                        const tempitem = {of: currentlist.map((item) => dcopy(item))};
                        const finalitem = {};
                        const postitem = {};
                        let aggregated = null;
                        let afteraggregated = 0;
                        let finalvalue = null;
                        let includeof = true;
                        segment.forEach((arg, propx) => {
                            if (labels?.[propx]) {
                                arg.label = labels[propx].label;
                                arg.subop = labels[propx].subop;
                            }
                            if (arg.subop == '~' && arg.label == null && arg.value == null) {
                                includeof = false
                            } else if (!aggregated && arg.subop?.includes('~') && dtype(arg.value, 'string')) {
                                tempitem[arg.label || '_' + (propx + 1).toString()] = arg.value;
                            } else if (!aggregated && arg.subop?.includes('~') && Array.isArray(arg.value)) {
                                const subresult = this.execute(currentlist, arg.value.map((x) => structuredClone(x)), labeled);
                                tempitem[arg.label || '_' + (propx + 1).toString()] = subresult.length > 0 ? getname(subresult[0]) : null;
                            } else {
                                const prop = arg.label || (typeof arg.value === 'string' ? arg.value : null) || '_' + (propx + 1).toString();
                                const aggname = (arg.label == null && typeof arg.value == 'string' && arg.value) || (arg.label && arg.subop == null && arg.value == null);
                                const firstitem = currentlist[0];
                                const isprop = typeof firstitem == 'object' && prop in firstitem;
                                if ((arg.subop == '=' || !isprop) && arg.subop != '~' && aggname in this.annotators) {
                                    const aggval = this.annotators[aggname](tempitem);
                                    const agglabel = arg.label || aggname;
                                    if (aggval != null) finalitem[agglabel] = aggval;
                                    aggregated = agglabel;
                                    finalvalue = dcopy(aggval);
                                } else if (!aggregated) {
                                    let propitems;
                                    if (Array.isArray(arg.value) || !arg.label) {
                                        const propres = this.execute(tempitem.of, Array.isArray(arg.value) ? arg.value.map((x) => structuredClone(x)) : [{operator: '..', args: [{value: arg.value}]}], labeled);
                                        if (arg.subop?.includes('~')) {
                                            propitems = getname(propres[0]);
                                        } else {
                                            propitems = propres;
                                        }
                                    } else {
                                        if (arg.subop?.includes('~')) {
                                            propitems = arg.value;
                                        } else {
                                            propitems = [arg.value]
                                        }
                                    }
                                    tempitem[prop] = propitems;
                                } else if (aggregated in finalitem && Array.isArray(finalitem[aggregated]) && finalitem[aggregated].length > afteraggregated) {
                                    const propval = finalitem[aggregated][afteraggregated];
                                    const mappedval = arg.subop?.endsWith('~') ? propval : [propval];
                                    tempitem[prop] = mappedval;
                                    postitem[prop] = mappedval;
                                    afteraggregated++;
                                }
                            }
                        });
                        if (op.operator == '....' && afteraggregated > 0) {
                            outputlist.push(postitem);
                        } else if (op.operator == '....' && finalvalue != null) {
                            if (dtype(finalvalue, 'array')) {
                                outputlist = outputlist.concat(finalvalue);
                            } else {
                                outputlist.push(finalvalue);
                            }
                        } else {
                            for (const prop in tempitem) {
                                if (!(prop in finalitem) && prop != 'of') finalitem[prop] = tempitem[prop];
                            }
                            if (includeof) finalitem.of = tempitem.of;
                            outputlist.push(finalitem);
                        }
                    }
                    break;
                case '|': // annotate
                    outputlist = currentlist.slice(0).map((item) => dcopy(item));
                    op.args.filter((arg) => arg.subop?.includes('>') && arg.label != null && arg.label != undefined && Array.isArray(arg.value))
                        .forEach((arg) => (labeled['=>'] ??= {})[arg.label] = arg.value);
                        
                    const annotatetimer = {};
                    op.args.filter((arg) => arg.subop?.endsWith('@')).forEach((arg) => arg.counter = undefined);
                    for (let i=0; i<outputlist.length; i++) {
                        this.timecheck(annotatetimer, i, outputlist.length, op);
                        const baseitem = outputlist[i];
                        if (typeof baseitem != 'object') {
                            outputlist[i] = {};
                            if (op.args?.[0]?.value != '_') outputlist[i].name = baseitem;
                        }
                        const item = outputlist[i];
                        const newprops = op.args.filter((arg) => arg.subop != '<' && (arg.label || !arg.subop?.includes('-'))).map((arg) => arg.label || arg.value);
                        if (typeof item == 'object') {
                            let argx = -1;
                            for (const arg of op.args) {
                                argx++;
                                if (arg?.subop == '<' && dtype(arg.value, 'string') && argx < op.args.length - 1) {
                                    if (arg.value in item && (Array.isArray(item[arg.value]) || dtype(item[arg.value], 'object'))) {
                                        const sublabeled = {...labeled};
                                        if (arg.label) sublabeled[arg.label] = [item];
                                        item[arg.value] = this.execute(item[arg.value], [{operator: op.operator, args: op.args.slice(argx + 1).map((arg) => structuredClone(arg))}], sublabeled);
                                    }
                                    break;
                                } else if (arg.separator == ';' && argx == op.args.length - 1 && arg.label == null && arg.value == null && (arg.subop == null || arg.subop == '~')) {
                                    for (const oldprop in item) {
                                        if (!(newprops.includes(oldprop))) {
                                            const tempval = item[oldprop];
                                            delete item[oldprop];
                                            if (arg.subop != '~') item[oldprop] = tempval;
                                        }
                                    }
                                } else if (arg.subop?.endsWith('-')) {
                                    if (arg.value && typeof arg.value == 'string' && arg.value in item) {
                                        if (arg.label && typeof arg.label == 'string') item[arg.label] = item[arg.value];
                                        delete item[arg.value];
                                    } else if (arg.label && Array.isArray(arg.value)) {
                                        const toremoveids = new Set(step(item, arg.value, null, labeled).map((subitem) => getid(subitem)));
                                        item[arg.label] = item[arg.label].filter((subitem) => !toremoveids.has(getid(subitem)));                                  
                                    }
                                } else if (argx == 0 && arg.label != null && arg.value == '_') {
                                    item[arg.label] = [baseitem];
                                } else {
                                    if (!arg.label && arg.value && typeof arg.value === 'string' && arg.value != '') {
                                        if (arg.subop == '=' && arg.value in this.annotators) {
                                            item[arg.value] = this.annotators[arg.value](item);
                                        } else if (arg.value in item) {
                                            const moveprop = arg.value;
                                            const value = structuredClone(item[moveprop]);
                                            delete item[moveprop];
                                            item[moveprop] = value;
                                        } else if (arg.value in this.annotators) {
                                            item[arg.value] = this.annotators[arg.value](item);
                                        }
                                    }
                                    if (arg.label) {
                                        let vals = [];
                                        if (arg.subop.endsWith('@')) {
                                            if (arg.subop.endsWith('@@')) {
                                                arg.counter ??= arg.value == null ? outputlist.length + 1 : 0;
                                            } else {
                                                arg.counter ??= 1;
                                                vals = [arg.counter];
                                            }
                                            if (Array.isArray(arg.value)) {
                                                arg.counter += step(item, arg.value, null, labeled).length;
                                            } else if (dtype(arg.value, 'string') && dtype(item[arg.value], 'number')) {
                                                arg.counter += Number(item[arg.value]);
                                            } else {
                                                arg.counter += arg.subop.endsWith('@@') ? -1 : 1;
                                            }
                                            if (arg.subop.endsWith('@@')) {
                                                vals = [arg.counter];
                                            }
                                        } else if (arg.subop == '=' && typeof arg.value == 'string' && arg.value in this.annotators) {
                                            vals = [this.annotators[arg.value](item)];
                                        } else {
                                            vals = step(item, arg.value, arg.subop, labeled);
                                        }
                                        const scalar = arg.subop?.includes('~') || arg.subop?.endsWith('@') || (arg.subop == '=' && typeof arg.value == 'string' && (arg.value in this.annotators || arg.value.startsWith('=')));
                                        const base = (arg.subop?.endsWith('+') && arg.label in item && Array.isArray(item[arg.label])) ? item[arg.label] : [];
                                        if (arg.label == '_') {
                                            const sourceitem = dtype(vals, 'array') ? vals[0] : val;
                                            if (dtype(sourceitem, 'object')) {
                                                for (const prop in sourceitem) {
                                                    if (!(prop in item)) {
                                                        if (scalar && Array.isArray(sourceitem[prop])) {
                                                            item[prop] = sourceitem[prop][0];
                                                        } else {
                                                            item[prop] = sourceitem[prop];
                                                        }
                                                    }
                                                }
                                            }
                                        } else if (dtype(vals, 'array')) {
                                            if (scalar && vals.length > 0) {
                                                item[arg.label] = getname(vals[0]);
                                            } else {
                                                item[arg.label] = base.concat(vals.map((v) => dcopy(v)));
                                            }
                                        } else if (vals) {
                                            item[arg.label] = base.concat(vals);
                                        }
                                    }
                                }
                            }
                        }
                    }
                    break;
                case '???':
                    outputlist = currentlist;
                    const commentval = op.args?.[0]?.value;
                    if (commentval in labeled) console.log({[commentval]: labeled[commentval].slice(0)});
                    if (commentval == 'end') return (this.debug && !inputlist) ? operations : outputlist;
                    if (commentval == 'recache') this.recache = new Set();
                    if (typeof currentlist[0] == 'object' && op.args?.[0]?.value in currentlist[0] && !op.args?.[0]?.label) console.log({[op.args?.[0]?.value]: currentlist[0][op.args?.[0]?.value]});
                    break;
                default:
                    outputlist = []
            }
            currentlist = outputlist.slice(0);
            if (toplevel) {
                if (!this.adaptive) {
                    op.completed = true;
                    operations[0].progress = currentlist;
                    operations[0].labeled = labeled;
                }
                if (this.debug) {
                    op.results = outputlist.slice(0, typeof this.debug == 'number' ? this.debug : outputlist.length);
                    op.time = (performance.now() - opstart) / 1000;
                }
            }
        }
        return (this.debug && !inputlistraw) ? operations : outputlist;
    }

    gettype(value) {
        if (value == null || value == undefined) return null;
        const trylist = [value];
        if (this.features.plurality) {
            if (value.endsWith('s')) {
                trylist.push(value.slice(0, value.length - 1));
                trylist.push(value + 'es');
            } else {
                trylist.push(value + 's');
            }
        }
        for (const tryval of trylist) {
            if (tryval in this.data) {
                return this.data[tryval];
            } else if (this.internal_datasets.includes(tryval)) {
                return [];
            }
        }
        if (this.savedquerynames.has(value)) {
            const savedqueries = this.data.queries.filter((q) => q.name == value);
            if (savedqueries?.length == 1) {
                const sq = savedqueries[0];
                if (!sq.results) sq.results = this.executeq(sq.query);
                return sq.results.slice(0);
            }
        }
        return null;
    }

    dcopy = (item) => {
        if (this.dtype(item, 'literal')) return item;
        if (this.dtype(item, 'array')) return item.slice(0);
        return Object.assign({}, item);
    }
    
    dtype(item, test=null) {
        const t = typeof item;
        const type = t === 'object' ? (item === null ? null : Array.isArray(item) ? 'array' : 'object')
            : (t === 'string' || t === 'number' || t === 'boolean') ? 'literal'
            : null;
        if (!test) return type;
        if (test === 'number') return type === 'literal' && !isNaN(item);
        if (test === 'string') return t === 'string';
        if (test === 'boolean') return t === 'boolean';
        return type === test;
    }
    
    nullish(val) {
        return val === false || val === null || val === undefined || val === '' || (Array.isArray(val) && val.length == 0) || (typeof val == 'object' && Object.keys(val).length == 0);
    }

    getname = (item) => {
        if (typeof item === 'object' && item != null) {
            if (item?.name != null) {
                return item.name;
            } else if (item?.key?.length > 0) {
                return item.key.join(' / ');
            }
            if (this.features.guessname) {
                const {id, ...nonidprops} = item;
                const names = Object.values(nonidprops).filter((val) => typeof val === 'string' || typeof val === 'number');
                if (names.length > 0) {
                    return names[0];
                }
                for(const key in nonidprops) {
                    if (Array.isArray(item[key]) && item[key].length == 1 && typeof item[key][0] == 'string') {
                        return item[key][0];
                    }
                }
            }
        } else if (typeof item === 'string' || typeof item === 'number') {
            return item;
        } else if (typeof item === 'boolean') {
            return item.toString();
        }
        return '';
    }
    
    getid = (item) => {
        if (typeof item === 'object' && item != null) {
            const iditems = (item.id || item.uri || item.name) ? [item] : (item.key?.length > 0 && !item.keyindex) ? item.key : Object.keys(item).length == 3 && item.of?.length > 0 ? item.of : [item];
            return iditems.map((item) => {
                if (item.id) {
                    if (Array.isArray(item.id)) {
                        return item.id[0];
                    } else {
                        return item.id;
                    }
                } else if (item.uri) {
                    if (Array.isArray(item.uri)) {
                        return item.uri[0];
                    } else {
                        return item.uri;
                    }
                } else if (this.features.guessid && item.name) {
                    return item.name;
                } else {
                    const stringifiedid = JSON.stringify(item);
                    // if (stringifiedid.length > 128) console.warn({idstringify: item, idlength: stringifiedid.length});
                    return stringifiedid;
                }
            }).join(',');
        } else if (typeof item === 'string' || typeof item === 'number') {
            return item;
        }
        return null;
    }
    
    step = (item, property, subop, labeled) => {
        // (this.data.trace ??= []).push({stepitem: item, property: property, subop: subop, labeled: labeled});
        const dtype = this.dtype;
        const dcopy = this.dcopy;
        const resolve = this.resolve;
        const escapeRegExp = this.escapeRegExp;
        const vals = [];
        if (subop?.includes('~') && dtype(property, 'literal')) {
            vals.push(property);
        } else if (Array.isArray(property)) {
            this.execute([item], property.map((x) => structuredClone(x)), labeled).flatMap((x) => x).forEach((val) => vals.push(val));
        } else if (property == '_') {
            vals.push(dcopy(item));
        } else if (item?.of && dtype(property, 'number')) {
            const propval = Number(property);
            (subop == '-' || propval < 0 ? item.of.slice(-1 * Math.abs(Number(property))) : item.of.slice(0, Math.abs(Number(property)))).forEach((val) => vals.push(val));
        } else if (property == 'id' && dtype(item, 'literal')) {
            vals.push(item);
        } else if (property == 'name') {
            vals.push(this.getname(item));
        } else if (this.features.inlinemath && (dtype(item, 'number') || dtype(item, 'object')) && property.startsWith('=')) {
            let calculation = property.slice(1);
            const mathwords = Object.getOwnPropertyNames(Math).filter((mathword) => mathword.match(/^[a-z0-9]+$/)).sort((a, b) => b.length - a.length || a.localeCompare(b));
            const otherwords = ['split'];
            const variables = Object.entries(item).concat(Object.entries(labeled))
                .map(([k, v]) => k)
                .sort((a, b) => b.length - a.length || a.localeCompare(b));
            if (dtype(this.getname(item), 'number')) variables.push('_');
            const allowedwords = mathwords.concat(otherwords).concat(variables).sort((a, b) => b.length - a.length || a.localeCompare(b));
            const allowed = new RegExp(`^((\\b(${allowedwords.map((w) => escapeRegExp(w)).join('|')})\\b)|([0-9_\\+\\/\\*\\(\\)\\[\\]\\.%=,'" -]*))*$`);
            let val;
            if (calculation.match(allowed)) {
                for (const variable of variables) {
                    const variableex = new RegExp(`\\b${variable}\\b`, 'g');
                    if (calculation.match(variableex)) {
                        let vval = variable == '_' ? Number(this.getname(item)) : item[variable] ?? labeled[variable];
                        if (Array.isArray(vval) && vval.length == 1) vval = vval[0];
                        calculation = calculation.replaceAll(variableex, dtype(vval, 'number') ? vval : JSON.stringify(vval));
                    }
                }
                if (calculation.match(/[A-Za-z]/)) {
                    for (const mathword of mathwords) {
                        const mathwordex = new RegExp(`\\b${mathword}\\b`, 'g');
                        calculation = calculation.replaceAll(mathwordex, `Math.${mathword}`);
                        if (!calculation.match(/[A-Za-z]/)) break;
                    }
                }
                calculation = calculation.replaceAll(/\b=\b/g, '==');
                try {
                    val = eval(calculation);
                } catch (error) {
                    val = calculation;
                }
            } else {
                val = calculation;
            }
            if (val !== false) vals.push(val);
        } else if (item) {
            let found = false;
            if (property != null && !dtype(item, 'literal')) {
                const tryvals = [property];
                if (this.features.plurality) {
                    if (!property.endsWith('s') && !this.internal_datasets.includes(property + 's')) tryvals.push(property + 's');
                    if (property.endsWith('s') && !this.internal_datasets.includes(property)) tryvals.push(property.replace(/s$/, ''));
                }
                if (this.features.unscore && property.match(/ /)) tryvals.push(property.replaceAll(/ /g, '_'));
                for (const tryval of tryvals) {
                    if (tryval in item) {
                        const resolved = resolve(tryval, item[tryval], labeled);
                        if (Array.isArray(resolved)) {
                            resolved.forEach((x) => vals.push(x));
                        } else {
                            vals.push(resolved);
                        }
                        found = true;
                        break;
                    }
                }
            }
            if (!found && labeled?.['=>']?.[property]) {
                this.execute([item], labeled['=>'][property], labeled).forEach((val) => vals.push(val));
                found = true;
            }
            if (!found && (property in this.data || property in this.adapters || this.savedquerynames.has(property) || property in labeled)) {
                const typenav = resolve(property, item, labeled, true);
                if (typenav) {
                    (Array.isArray(typenav) ? typenav : [typenav]).forEach((t) => vals.push(t));
                }
            }
            if (!found && property.includes('→')) {
                this.step(item, property.split(/\s*→\s*/).map((part) => ({operator: '..', args: [{value: part}]}))).forEach((val) => vals.push(val));
            }
        }
        return vals;
    }

    indexlogit = (property, type) => {
        const today = new Date().toISOString().slice(0, 10);
        ((this.index._ ||= {})[property] ||= {})[today] ||= {read: 0, write: 0};
        this.index._[property][today][type]++;
        this.index_modified.add('_');
        if (type == 'write') this.index_modified.add(property);
    }
        
    resolve = (property, item, labeled, navigate = false) => {
        const dtype = this.dtype;
        const aqueue = this.aqueue;
        if (dtype(item, 'object')) {
            if (this.adapters?.[property]?.annotator) {
                if ((!this.recache || this.recache.has(property)) && property in this.index && this.getid(item) in this.index[property]) {
                    this.indexlogit(property, 'read');
                    return this.index[property][this.getid(item)];
                } else {
                    return aqueue(property, item);
                }
            } else if (navigate) {
                const relative_query = this.savedquerynames.has(property) && this.data.queries.find((q) => q.relative && q.name == property);
                if (relative_query) {
                    return this.step(item, this.parse(relative_query.query), null, labeled);
                }
                if ('id' in item || 'uri' in item) {
                    return this.resolve(property, this.getid(item), labeled);
                }
                return this.getname(item);
            } else {
                return item;
            }
        } else if (dtype(item, 'literal') && typeof property == 'string' && (property in labeled || this.destinations.has(property) || (this.features.plurality && !this.internal_datasets.includes(property + 's') && this.destinations.has(property + 's')))) {
            if (typeof property == 'string') {
                if ((!this.recache || this.recache.has(property)) && property in this.index && item in this.index[property]) {
                    this.indexlogit(property, 'read');                   
                    return this.index[property][item];
                }
                if (this.features.autoresolve) {
                    const typeitems = labeled[property] ?? this.gettype(property);
                    if (Array.isArray(typeitems)) {
                        for (const lookupkey of ['id', 'uri', 'name', property, property + 's']) {
                            const found = typeitems.filter((typeitem) => {
                                return dtype(typeitem, 'object') && (lookupkey in typeitem) && (typeitem[lookupkey] == item || (Array.isArray(typeitem[lookupkey]) && typeitem[lookupkey].length == 1 && typeitem[lookupkey][0] == item));
                            });
                            if (found.length == 1) {
                                (this.index[property] ||= {})[item] = found[0];
                                this.indexlogit(property, 'write');
                                return found[0];
                            }
                        }
                        if (property in this.adapters && (!this.recache || this.recache.has(property))) {
                            return aqueue(property, item);
                        }
                        return null;
                    }
                }
                if (property in this.adapters) {
                    return aqueue(property, item);
                }
            }
        }
        return item;
    }
    
    aqueue = (property, item) => {
        const pending_queues = Object.keys(this.adapters).filter((key) => this.adapters[key].queue.length > 0);
        if (pending_queues.length == 0 || (pending_queues.length == 1 && pending_queues[0] == property)) {
            this.adapters[property].queue.push(this.adapters[property].annotator ? item : this.getid(item));
        }
        this.adaptive = true;
        return undefined;
    }

    samearray(a, b) {
        if ((a && !b) || (!a && b) || a.length != b.length) return false;
        for(let x=0; x<a.length; x++) {
            if (this.getid(a[x]) != this.getid(b[x])) return false;
        }
        return true;
    }
    
    verify = async (i = null) => {
        let toverify = this.data.queries;
        if (i && !isNaN(i)) toverify = toverify.slice(i - 1, i);
        let allsame = true;
        for (const savedq of toverify) {
            console.log('verifying ' + savedq.name);
            const testres = await this.query(savedq.query);
            if (testres.length != savedq.results.length) {
                console.log('--x result count changed from ' + savedq.results.length + ' to ' + testres.length);
                allsame = false;
            } else {
                for (let i=0; i<testres.length; i++) {
                    const testrow = testres[i];
                    const savedrow = savedq.results[i];
                    if (typeof savedrow == 'object') {
                        for (const prop in savedrow) {
                            if (!(prop in testrow)) {
                                console.log('--x row ' + (i+1) + ': new results missing property ' + prop);
                                allsame = false;
                            } else {
                                const testval = JSON.stringify(testrow[prop]);
                                const savedval = JSON.stringify(savedrow[prop]);
                                if (testval != savedval) {
                                    console.log('--x row ' + (i+1) + ': different value for property ' + prop);
                                    console.log({was: savedrow[prop], now: testrow[prop]})
                                    allsame = false;
                                }
                            }
                        }
                    } else {
                        if (testrow != savedrow) {
                            console.log('--x row ' + (i+1) + ': different value');
                            console.log({was: savedrow, now: testrow});
                            allsame = false;
                        }
                    }
                }
            }
            if (allsame) console.log('--- results unchanged');
        }
    }
    
    index_check() {
        console.table(Object.entries(this.index).map(([key, vals]) => ({key: key, vals: Object.keys(vals).length, size: Object.keys(vals).length * JSON.stringify(Object.entries(vals).slice(0, 1)).length})).sort((a, b) => b.size - a.size || b.vals - a.vals || a.key.localeCompare(b.key)))
    }
    
    index_materialize() {
        this.load(Object.keys(this.index).flatMap((i) => Object.keys(this.index[i]).flatMap((k) => ({index: i, indexed: k, value: this.index[i][k]}))), 'index contents') 
    }
    
    indexlog_materialize() {
        this.load(Object.entries(dactal.index._).map(([k, v]) => ({index: k, log: Object.entries(v).map(([date, readwrite]) => ({date: date, read: readwrite.read, write: readwrite.write}))})), 'indexlog');
    }

    queries_check() {
        console.table(dactal.data.queries.map((q) => ({queryname: q.name, results: q.results?.length || 0, size: q.results?.length > 0 ? q.results.length * JSON.stringify(q.results[0]).length : 0})).sort((a, b) => b.size - a.size || b.results - a.results || a.queryname.localeCompare(b.queryname)))
    }
    
    data_check() {
        console.table(Object.entries(this.data).map(([key, vals]) => ({key: key, vals: Object.keys(vals).length, size: Object.keys(vals).length * JSON.stringify(Object.entries(vals).slice(0, 1)).length})).sort((a, b) => b.size - a.size || b.vals - a.vals || a.key.localeCompare(b.key)))
    }
}

window.DACTAL = new DACTAL();