<?php
$need = ['phalcon','gd','mysqli','pdo_mysql','pdo_pgsql','pcntl','sockets','bcmath','exif','intl','zip','redis','memcached','apcu','yaml','igbinary','msgpack','event','ev','Zend OPcache','session','sodium'];
$missing = array_values(array_filter($need, fn($e) => !extension_loaded($e)));
echo json_encode(['php' => PHP_VERSION, 'zts' => PHP_ZTS, 'sapi' => PHP_SAPI, 'missing' => $missing,
  'phalcon' => phpversion('phalcon'), 'opcache' => opcache_get_status(false)['opcache_enabled'] ?? false,
  'display_errors' => ini_get('display_errors'), 'expose_php' => ini_get('expose_php'), 'post_max_size' => ini_get('post_max_size'),
  'tz' => date_default_timezone_get()]), "\n";
