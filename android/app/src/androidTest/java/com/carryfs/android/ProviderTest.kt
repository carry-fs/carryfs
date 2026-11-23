package com.carryfs.android

import android.graphics.Bitmap
import android.graphics.Color
import android.graphics.Point
import android.net.Uri
import android.provider.DocumentsContract
import android.provider.DocumentsContract.Document
import androidx.test.platform.app.InstrumentationRegistry
import org.junit.After
import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertThrows
import org.junit.Assert.assertTrue
import org.junit.Before
import org.junit.Test
import java.io.File
import java.io.FileNotFoundException

/**
 * Drives the documents provider the way other apps do, on a copy of
 * files/test.img: a desktop-made image holding /hello.txt. See
 * android/README.md for how to push it.
 */
class ProviderTest {
    private val context = InstrumentationRegistry.getInstrumentation().targetContext
    private val resolver = context.contentResolver
    private val image = File(context.cacheDir, "work.img")

    @Before
    fun mount() {
        File(context.filesDir, "test.img").copyTo(image, overwrite = true)
        Drives.mountImage(context, image)
    }

    @After
    fun eject() {
        Drives.eject(context, Drives.IMAGE)
        image.delete()
    }

    private fun root(): Uri {
        val volume = Drives.volumes().single()
        return DocumentsContract.buildDocumentUri(CarryProvider.AUTHORITY, CarryProvider.documentId(volume, Native.ROOT))
    }

    private fun names(folder: Uri): List<String> {
        val children = DocumentsContract.buildChildDocumentsUri(
            CarryProvider.AUTHORITY, DocumentsContract.getDocumentId(folder),
        )
        return resolver.query(children, arrayOf(Document.COLUMN_DISPLAY_NAME), null, null, null)!!.use {
            generateSequence { if (it.moveToNext()) it.getString(0) else null }.toList().sorted()
        }
    }

    @Test
    fun readsWhatADesktopWrote() {
        val hello = names(root()).single { it == "hello.txt" }
        val children = DocumentsContract.buildChildDocumentsUri(
            CarryProvider.AUTHORITY, DocumentsContract.getDocumentId(root()),
        )
        val id = resolver.query(children, arrayOf(Document.COLUMN_DOCUMENT_ID, Document.COLUMN_DISPLAY_NAME), null, null, null)!!
            .use { generateSequence { if (it.moveToNext()) it.getString(0) to it.getString(1) else null }.first { it.second == hello }.first }
        val uri = DocumentsContract.buildDocumentUri(CarryProvider.AUTHORITY, id)
        assertEquals("hello from a desktop\n", resolver.openInputStream(uri)!!.use { it.readBytes().decodeToString() })
    }

    @Test
    fun grantsFromAnotherNamespaceCannotOpenCurrentDocuments() {
        val file = DocumentsContract.createDocument(resolver, root(), "text/plain", "note")!!
        val id = DocumentsContract.getDocumentId(file)
        for (expired in listOf(id.substringAfter(':'), "old:${id.substringAfter(':')}")) {
            val uri = DocumentsContract.buildDocumentUri(CarryProvider.AUTHORITY, expired)
            assertThrows(FileNotFoundException::class.java) { resolver.openInputStream(uri)?.close() }
        }
    }

    @Test
    fun filesRoundTripAndSurviveRemount() {
        val root = root()
        val folder = DocumentsContract.createDocument(resolver, root, Document.MIME_TYPE_DIR, "Docs")!!
        val file = DocumentsContract.createDocument(resolver, folder, "text/plain", "note")!!
        val data = ByteArray(3 shl 20) { (it % 251).toByte() }
        resolver.openOutputStream(file)!!.use { it.write(data) }
        assertArrayEquals(data, resolver.openInputStream(file)!!.use { it.readBytes() })
        assertEquals(listOf("note.txt"), names(folder))

        val renamed = DocumentsContract.renameDocument(resolver, file, "kept.txt") ?: file
        val moved = DocumentsContract.moveDocument(resolver, renamed, folder, root)!!
        DocumentsContract.deleteDocument(resolver, folder)
        assertEquals(listOf("hello.txt", "kept.txt"), names(root))

        // Eject publishes everything; a fresh mount must read the same bytes.
        Drives.eject(context, Drives.IMAGE)
        Drives.mountImage(context, image)
        val remounted = DocumentsContract.buildDocumentUri(
            CarryProvider.AUTHORITY,
            CarryProvider.documentId(Drives.volumes().single(), DocumentsContract.getDocumentId(moved).substringAfter('/').toLong()),
        )
        assertArrayEquals(data, resolver.openInputStream(remounted)!!.use { it.readBytes() })
    }

    @Test
    fun picturesHavePreviews() {
        val photo = DocumentsContract.createDocument(resolver, root(), "image/jpeg", "photo.jpg")!!
        val picture = Bitmap.createBitmap(1200, 800, Bitmap.Config.ARGB_8888).apply { eraseColor(Color.RED) }
        resolver.openOutputStream(photo)!!.use { picture.compress(Bitmap.CompressFormat.JPEG, 90, it) }

        val flags = resolver.query(photo, arrayOf(Document.COLUMN_FLAGS), null, null, null)!!
            .use { it.moveToFirst(); it.getInt(0) }
        assertTrue(flags and Document.FLAG_SUPPORTS_THUMBNAIL != 0)
        val preview = DocumentsContract.getDocumentThumbnail(resolver, photo, Point(256, 256), null)!!
        assertEquals(256 to 171, preview.width to preview.height)
    }
}
