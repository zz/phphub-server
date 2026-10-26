<?php
header('Content-Type: application/json');
echo json_encode([
    'info3' => true,
    'script_filename' => $_SERVER['SCRIPT_FILENAME'],
    'script_name' => $_SERVER['SCRIPT_NAME'],
    'php_self' => $_SERVER['PHP_SELF'],
    'document_uri' => $_SERVER['DOCUMENT_URI'],
    'path_info' => $_SERVER['PATH_INFO'] ?? null,
    'request_uri' => $_SERVER['REQUEST_URI'],
    'query_string' => $_SERVER['QUERY_STRING'],
    'get' => $_GET,
]);
