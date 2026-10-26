<?php
// Unmodified front controller: runs once per request.
$GLOBALS['counter'] = ($GLOBALS['counter'] ?? 0) + 1;
header('Content-Type: application/json');
echo json_encode(['classic' => true, 'counter' => $GLOBALS['counter'], 'get' => $_GET, 'uri' => $_SERVER['REQUEST_URI'], 'sapi' => PHP_SAPI]);
