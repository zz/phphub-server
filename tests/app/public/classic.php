<?php
// Unmodified front controller: runs once per request.
if (isset($_GET['sleep'])) usleep((int)$_GET['sleep'] * 1000);
if (isset($_GET['exit'])) { echo 'bye'; exit((int)$_GET['exit']); }
if (isset($_GET['throw'])) throw new RuntimeException('boom');
$GLOBALS['counter'] = ($GLOBALS['counter'] ?? 0) + 1;
header('Content-Type: application/json');
echo json_encode(['classic' => true, 'counter' => $GLOBALS['counter'], 'get' => $_GET, 'uri' => $_SERVER['REQUEST_URI'], 'script_filename' => $_SERVER['SCRIPT_FILENAME'], 'path_info' => $_SERVER['PATH_INFO'] ?? null, 'sapi' => PHP_SAPI]);
