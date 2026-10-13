package com.fortis.wallet.data

import kotlinx.coroutines.TimeoutCancellationException
import kotlinx.coroutines.launch
import kotlinx.coroutines.runBlocking
import kotlinx.coroutines.withTimeout
import okhttp3.OkHttpClient
import okhttp3.Request
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test
import java.net.ServerSocket
import java.util.concurrent.TimeUnit
import kotlin.concurrent.thread

class HttpSendTest {
    /** A server that accepts and reads, but never answers. */
    private fun silentServer(): ServerSocket = ServerSocket(0).also { ss ->
        thread(isDaemon = true) {
            runCatching {
                while (true) {
                    val s = ss.accept()
                    thread(isDaemon = true) { runCatching { s.getInputStream().readBytes() } }
                }
            }
        }
    }

    @Test
    fun cancelling_a_call_aborts_it_at_once_not_after_its_read_timeout() {
        silentServer().use { ss ->
            val http = OkHttpClient.Builder().readTimeout(30, TimeUnit.SECONDS).build()
            val req = Request.Builder().url("http://127.0.0.1:${ss.localPort}/x").build()
            val started = System.nanoTime()
            val timedOut = runCatching {
                runBlocking { withTimeout(300) { http.send(req) { it.code } } }
            }.exceptionOrNull()
            val ms = (System.nanoTime() - started) / 1_000_000
            assertTrue("timed out: $timedOut", timedOut is TimeoutCancellationException)
            assertTrue("aborted in $ms ms", ms < 5_000)
            // The dispatcher drops a cancelled call asynchronously, a moment later.
            val until = System.nanoTime() + 2_000_000_000L
            while (http.dispatcher.runningCallsCount() > 0 && System.nanoTime() < until) Thread.sleep(10)
            assertEquals("no call left running", 0, http.dispatcher.runningCallsCount())
        }
    }

    @Test
    fun a_call_returns_what_read_makes_of_the_response() {
        ServerSocket(0).use { ss ->
            thread(isDaemon = true) {
                ss.accept().use { s ->
                    s.getInputStream().bufferedReader().let { r -> while (r.readLine()?.isNotEmpty() == true) {} }
                    s.getOutputStream().write("HTTP/1.1 201 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nhi".toByteArray())
                }
            }
            val http = OkHttpClient()
            val req = Request.Builder().url("http://127.0.0.1:${ss.localPort}/x").build()
            val got = runBlocking { http.send(req) { it.code to it.body!!.string() } }
            assertEquals(201 to "hi", got)
        }
    }

    @Test
    fun a_response_that_arrives_just_as_the_call_is_cancelled_is_closed_not_leaked() {
        ServerSocket(0).use { ss ->
            thread(isDaemon = true) {
                ss.accept().use { s ->
                    s.getInputStream().bufferedReader().let { r -> while (r.readLine()?.isNotEmpty() == true) {} }
                    s.getOutputStream().write("HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nhi".toByteArray())
                    Thread.sleep(2_000)
                }
            }
            val headers = java.util.concurrent.CountDownLatch(1)
            val released = java.util.concurrent.CountDownLatch(1)
            val http = OkHttpClient.Builder().eventListener(object : okhttp3.EventListener() {
                override fun responseHeadersEnd(call: okhttp3.Call, response: okhttp3.Response) = headers.countDown()
                override fun connectionReleased(call: okhttp3.Call, connection: okhttp3.Connection) = released.countDown()
            }).build()
            // A dispatcher that holds the read until the test lets it go: the
            // cancellation lands between "the response arrived" and "the body is read".
            val held = java.util.concurrent.LinkedBlockingQueue<Runnable>()
            val gate = object : kotlinx.coroutines.CoroutineDispatcher() {
                override fun dispatch(context: kotlin.coroutines.CoroutineContext, block: Runnable) {
                    held.put(block)
                }
            }
            val req = Request.Builder().url("http://127.0.0.1:${ss.localPort}/x").build()
            runBlocking {
                val job = launch(kotlinx.coroutines.Dispatchers.Default) { http.send(req, io = gate) { it.body!!.string() } }
                assertTrue("response arrived", headers.await(5, TimeUnit.SECONDS))
                val read = held.poll(5, TimeUnit.SECONDS)
                assertTrue("the read was dispatched", read != null)
                job.cancel()
                read!!.run() // runs as cancelled: the read never happens
                kotlinx.coroutines.withTimeout(5_000) { job.join() }
            }
            assertTrue("the response was closed, releasing its connection", released.await(5, TimeUnit.SECONDS))
        }
    }

}
