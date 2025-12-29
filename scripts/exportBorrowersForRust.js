#!/usr/bin/env node
/**
 * Export borrowers from V7.5 format to Rust V8.0 format
 * 
 * Run: node scripts/exportBorrowersForRust.js
 * Output: liquidator-rs/data/borrowers.json
 */

const fs = require('fs');
const path = require('path');

// Paths
const DATA_DIR = path.join(__dirname, '..', 'data');
const OUTPUT_FILE = path.join(__dirname, '..', 'liquidator-rs', 'data', 'borrowers.json');

// Initialize output structure
const output = {
    aave: {},
    compound: {},
    venus: []
};

// Chain name mapping (V7.5 might use different names)
const chainMapping = {
    'base': 'base',
    'polygon': 'polygon',
    'arbitrum': 'arbitrum',
    'avalanche': 'avalanche',
    'bnb': 'bnb',
    'bsc': 'bnb',
};

console.log('🔍 Scanning for borrower data...\n');

// Check for borrowers.json (if it already exists in unified format)
const existingBorrowers = path.join(DATA_DIR, 'borrowers.json');
if (fs.existsSync(existingBorrowers)) {
    console.log('Found existing borrowers.json');
    try {
        const data = JSON.parse(fs.readFileSync(existingBorrowers, 'utf8'));
        
        // Copy Aave borrowers
        if (data.aave) {
            for (const [chain, borrowers] of Object.entries(data.aave)) {
                const normalizedChain = chainMapping[chain.toLowerCase()] || chain.toLowerCase();
                output.aave[normalizedChain] = borrowers;
                console.log(`  ${normalizedChain}: ${borrowers.length} Aave borrowers`);
            }
        }
        
        // Copy Compound borrowers
        if (data.compound) {
            output.compound = data.compound;
            const compoundCount = Object.values(data.compound).reduce((sum, markets) => 
                sum + Object.values(markets).reduce((s, b) => s + b.length, 0), 0);
            console.log(`  Compound: ${compoundCount} borrowers`);
        }
        
        // Copy Venus borrowers
        if (data.venus && Array.isArray(data.venus)) {
            output.venus = data.venus;
            console.log(`  Venus: ${data.venus.length} borrowers`);
        }
    } catch (e) {
        console.error('Error reading borrowers.json:', e.message);
    }
}

// Check for chain-specific files (aaveBorrowersBase.json, etc.)
const chainFiles = fs.readdirSync(DATA_DIR).filter(f => 
    f.includes('Borrowers') && f.endsWith('.json')
);

for (const file of chainFiles) {
    const filePath = path.join(DATA_DIR, file);
    console.log(`\nProcessing ${file}...`);
    
    try {
        const data = JSON.parse(fs.readFileSync(filePath, 'utf8'));
        
        // Extract chain name from filename
        // e.g., aaveBorrowersBase.json -> base
        const match = file.match(/borrowers?([A-Za-z]+)\.json/i);
        if (match) {
            let chain = match[1].toLowerCase();
            chain = chainMapping[chain] || chain;
            
            if (file.toLowerCase().includes('aave')) {
                // Aave format: array of addresses or object with borrowers array
                const borrowers = Array.isArray(data) ? data : (data.borrowers || []);
                if (!output.aave[chain]) {
                    output.aave[chain] = [];
                }
                // Merge and dedupe
                const existing = new Set(output.aave[chain]);
                for (const addr of borrowers) {
                    if (!existing.has(addr)) {
                        output.aave[chain].push(addr);
                    }
                }
                console.log(`  Added ${borrowers.length} Aave borrowers for ${chain}`);
            }
            
            if (file.toLowerCase().includes('compound')) {
                // Compound format varies
                const borrowers = Array.isArray(data) ? data : (data.borrowers || []);
                if (!output.compound[chain]) {
                    output.compound[chain] = {};
                }
                if (!output.compound[chain]['default']) {
                    output.compound[chain]['default'] = [];
                }
                output.compound[chain]['default'].push(...borrowers);
                console.log(`  Added ${borrowers.length} Compound borrowers for ${chain}`);
            }
            
            if (file.toLowerCase().includes('venus')) {
                const borrowers = Array.isArray(data) ? data : (data.borrowers || []);
                output.venus.push(...borrowers);
                console.log(`  Added ${borrowers.length} Venus borrowers`);
            }
        }
    } catch (e) {
        console.error(`  Error: ${e.message}`);
    }
}

// Also check for positions files that might have borrower addresses
const positionFiles = fs.readdirSync(DATA_DIR).filter(f => 
    f.includes('positions') && f.endsWith('.json')
);

for (const file of positionFiles) {
    const filePath = path.join(DATA_DIR, file);
    console.log(`\nExtracting borrowers from ${file}...`);
    
    try {
        const data = JSON.parse(fs.readFileSync(filePath, 'utf8'));
        
        // Extract chain from filename
        const chainMatch = file.match(/positions?[_-]?([A-Za-z]+)/i);
        if (chainMatch) {
            let chain = chainMatch[1].toLowerCase();
            chain = chainMapping[chain] || chain;
            
            // Positions format: array of {user: address, ...}
            if (Array.isArray(data)) {
                const borrowers = data.map(p => p.user || p.address || p.borrower).filter(Boolean);
                if (borrowers.length > 0) {
                    if (!output.aave[chain]) {
                        output.aave[chain] = [];
                    }
                    const existing = new Set(output.aave[chain]);
                    let added = 0;
                    for (const addr of borrowers) {
                        if (!existing.has(addr)) {
                            output.aave[chain].push(addr);
                            existing.add(addr);
                            added++;
                        }
                    }
                    if (added > 0) {
                        console.log(`  Extracted ${added} borrowers for ${chain}`);
                    }
                }
            }
        }
    } catch (e) {
        console.error(`  Error: ${e.message}`);
    }
}

// Dedupe all arrays
for (const chain of Object.keys(output.aave)) {
    output.aave[chain] = [...new Set(output.aave[chain])];
}
output.venus = [...new Set(output.venus)];

// Calculate totals
const aaveTotal = Object.values(output.aave).reduce((sum, arr) => sum + arr.length, 0);
const compoundTotal = Object.values(output.compound).reduce((sum, markets) => 
    sum + Object.values(markets).reduce((s, b) => s + b.length, 0), 0);
const venusTotal = output.venus.length;
const total = aaveTotal + compoundTotal + venusTotal;

console.log('\n' + '='.repeat(50));
console.log('📊 Summary:');
console.log(`   Aave: ${aaveTotal} borrowers across ${Object.keys(output.aave).length} chains`);
for (const [chain, borrowers] of Object.entries(output.aave)) {
    console.log(`     - ${chain}: ${borrowers.length}`);
}
console.log(`   Compound: ${compoundTotal} borrowers`);
console.log(`   Venus: ${venusTotal} borrowers`);
console.log(`   TOTAL: ${total} borrowers`);
console.log('='.repeat(50));

// Ensure output directory exists
const outputDir = path.dirname(OUTPUT_FILE);
if (!fs.existsSync(outputDir)) {
    fs.mkdirSync(outputDir, { recursive: true });
}

// Write output
fs.writeFileSync(OUTPUT_FILE, JSON.stringify(output, null, 2));
console.log(`\n✅ Exported to: ${OUTPUT_FILE}`);

if (total === 0) {
    console.log('\n⚠️  No borrowers found!');
    console.log('Your V7.5 bot may store borrowers differently.');
    console.log('Check your data/ folder and let me know the file structure.');
}
