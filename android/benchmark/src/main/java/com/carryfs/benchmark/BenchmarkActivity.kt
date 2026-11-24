package com.carryfs.benchmark

import android.app.Activity
import android.content.Intent
import android.net.Uri
import android.os.Bundle
import android.provider.DocumentsContract
import android.util.Log
import android.view.WindowManager
import android.widget.TextView
import java.util.UUID
import kotlin.concurrent.thread

/** An independent SAF client, so both providers receive calls across Binder. */
class BenchmarkActivity : Activity() {
    private companion object {
        const val TAG = "CarryFsBench"
        const val PICK_FOLDER = 1
        const val CARRY_PROVIDER = "com.carryfs.android.documents"
        const val STORAGE_PROVIDER = "com.android.externalstorage.documents"
    }

    private lateinit var status: TextView

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        window.addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
        status = TextView(this).apply {
            textSize = 18f
            setPadding(32, 32, 32, 32)
        }
        setContentView(status)
        val filesystem = intent.getStringExtra("filesystem")
        val volume = intent.getStringExtra("volume")
        val rounds = intent.getIntExtra("rounds", 3)
        if (filesystem !in listOf("carryfs", "exfat", "fat32") || rounds !in 1..100 ||
            (filesystem != "carryfs" && volume?.matches(Regex("[0-9A-Fa-f]{4}-[0-9A-Fa-f]{4}")) != true)
        ) {
            report("Set filesystem to carryfs, exfat or fat32, rounds to 1–100, and volume to the USB filesystem UUID for exFAT/FAT32.")
            return
        }
        report("Choose the benchmark folder on $filesystem.")
        if (savedInstanceState == null) {
            @Suppress("DEPRECATION")
            startActivityForResult(Intent(Intent.ACTION_OPEN_DOCUMENT_TREE).putExtra(Intent.EXTRA_LOCAL_ONLY, true), PICK_FOLDER)
        }
    }

    @Deprecated("Uses the platform picker without an AndroidX dependency")
    override fun onActivityResult(requestCode: Int, resultCode: Int, data: Intent?) {
        super.onActivityResult(requestCode, resultCode, data)
        if (requestCode != PICK_FOLDER) return
        val tree = data?.data
        if (resultCode != RESULT_OK || tree == null) {
            report("Folder selection cancelled.")
            return
        }
        if (!DocumentsContract.isTreeUri(tree)) {
            report("The picker did not return a folder grant.")
            return
        }
        val filesystem = checkNotNull(intent.getStringExtra("filesystem"))
        val volume = intent.getStringExtra("volume")
        val rounds = intent.getIntExtra("rounds", 3)
        val id = DocumentsContract.getTreeDocumentId(tree)
        val selectedTarget = if (filesystem == "carryfs") {
            tree.authority == CARRY_PROVIDER
        } else {
            tree.authority == STORAGE_PROVIDER && id.substringBefore(':').equals(volume, ignoreCase = true)
        }
        if (!selectedTarget) {
            report("Selected folder is on ${tree.authority} ($id). Select a folder on $filesystem.")
            return
        }
        val parent = DocumentsContract.buildDocumentUriUsingTree(tree, id)
        val runId = UUID.randomUUID().toString()
        thread(name = "usb-benchmark") {
            try {
                val workload = DocumentWorkload(contentResolver)
                val rows = buildString {
                    appendLine("disk\tlabel\tround\tphase\tmetric\tvalue\tunit\tstatus\treason")
                    for (round in 1..rounds) {
                        report("$filesystem round $round/$rounds")
                        workload.run(parent, "carryfs-bench-$runId-$round") { metric, value, unit ->
                            appendLine("usb\t$filesystem\t$round\tsaf\t$metric\t$value\t$unit\t\t")
                        }
                    }
                }
                val result = "results-$filesystem-$runId.tsv"
                openFileOutput(result, MODE_PRIVATE).bufferedWriter().use { it.write(rows) }
                report("Complete: $result")
            } catch (error: Exception) {
                val message = "Benchmark failed on $filesystem: ${error.message}"
                Log.e(TAG, message, error)
                runOnUiThread { status.text = message }
            }
        }
    }

    private fun report(message: String) {
        Log.i(TAG, message)
        runOnUiThread { status.text = message }
    }
}
