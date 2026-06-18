class DACTALdb {
	constructor(params={}) {
		this.dbname = params.dbname ??= 'DactalX';
		this.storename = params.storename ??= 'Data';
		this.db_promise = null;
		this.db = null;
		this.counts = {};
        this.persistent_query_history = false;
	}

	opendb() {
		if (!this.db_promise) {
			this.db_promise = new Promise((resolve, reject) => {
				// Open without version to check current state
				const checkRequest = indexedDB.open(this.dbname);

				checkRequest.onerror = (event) => {
					reject(event.target.error);
				};

				checkRequest.onsuccess = (event) => {
					const db = event.target.result;
					const currentVersion = db.version;
					const storeExists = db.objectStoreNames.contains(this.storename);

					if (storeExists) {
						// Store exists, just return this connection
						resolve(db);
					} else {
						// Need to create store, close and reopen with upgrade
						db.close();
						const newVersion = currentVersion + 1;
						const upgradeRequest = indexedDB.open(this.dbname, newVersion);

						upgradeRequest.onupgradeneeded = (event) => {
							const db = event.target.result;
							if (!db.objectStoreNames.contains(this.storename)) {
								db.createObjectStore(this.storename);
							}
						};

						upgradeRequest.onsuccess = (event) => {
							resolve(event.target.result);
						};

						upgradeRequest.onerror = (event) => {
							reject(event.target.error);
						};
					}
				};
			});
		}
		return this.db_promise;
	}

	async keys() {
		this.db ??= await this.opendb();
		return new Promise((resolve, reject) => {
			const tx = this.db.transaction(this.storename, 'readonly');
			const store = tx.objectStore(this.storename);
			const request = store.getAllKeys();

			request.onsuccess = function(event) {
				resolve(event.target.result);
			};

			request.onerror = function(event) {
				reject(event.target.error);
			};
		});
	}
    	
	countkeys(key, val) {
		let itemcount = null;
		if (Array.isArray(val)) {
			itemcount = val.length;
		} else if (typeof val == 'object') {
			itemcount = Object.keys(val).length;
		} else {
			itemcount = 1;
		}
		this.counts[key] = itemcount;
	}
	
	async set(key, val) {
        if (key == '_index') {
            for (const ikey in val) {
                await this.set('_' + ikey, val[ikey])
            }
            this.counts['_index'] = Object.keys(val).length;
        } else {
            if (key == 'queries') {
                val = val.map((v) => {
                    try {
                        structuredClone(v?.results?.[0]);
                        return v;
                    } catch (e) {
                        const {results, ...vrest} = v;
                        return vrest;
                    }
                })
            }
            this.db ??= await this.opendb();
            return new Promise((resolve, reject) => {
                const tx = this.db.transaction(this.storename, 'readwrite');
                const store = tx.objectStore(this.storename);
                // console.log({jsvall: JSON.stringify(val).length});
                // console.log({sc: structuredClone(val)});
                // console.log({jr: JSON.parse(JSON.stringify(val))});
                const request = store.put(val, key);
    
                request.onsuccess = () => {
                    this.countkeys(key, val);
                    resolve();
                };
    
                request.onerror = (event) => {
                    reject(event.target.error);
                };
            });
        }
	}

	async get(key) {
        if (key == '_index') {
            const indexdata = {};
            const allkeys = await this.keys();
            const ikeys = allkeys.filter((k) => k.startsWith('_'));
            if (ikeys.length > 1 || ikeys[0] != '_index') {
                for (const ikey of ikeys) {
                    if (ikey != '_index') indexdata[ikey.slice(1)] = await this.get(ikey);
                }
                return indexdata;
            }
        }
        this.db ??= await this.opendb();
        return new Promise((resolve, reject) => {
            const tx = this.db.transaction(this.storename, 'readonly');
            const store = tx.objectStore(this.storename);
            const request = store.get(key);

            request.onsuccess = (event) => {
                const result = event.target.result;
                if (result !== undefined) {
                    if (!(key in this.counts)) {
                        this.countkeys(key, result);
                    }
                    resolve(result);
                } else {
                    resolve(null);
                }
            };

            request.onerror = function(event) {
                reject(event.target.error);
            };
        });
	}

	async remove(key) {
		this.db ??= await this.opendb();
		return new Promise((resolve, reject) => {
			const tx = this.db.transaction(this.storename, 'readwrite');
			const store = tx.objectStore(this.storename);
			const request = store.delete(key);

			request.onsuccess = function() {
				resolve();
			};

			request.onerror = function(event) {
				reject(event.target.error);
			};
		});
	}
    
    async clear_index() {
        const dkeys = await this.keys();
        dkeys.forEach((k) => {
            if (k.startsWith('_')) {
                this.remove(k);
                console.log('removed ' + k);
            }
        })
    }

    async dbs() {
        return await indexedDB.databases();
    }
}	
